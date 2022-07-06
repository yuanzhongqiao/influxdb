//! Tool to clean up old object store files that don't appear in the catalog.

#![deny(
    rustdoc::broken_intra_doc_links,
    rust_2018_idioms,
    missing_debug_implementations,
    unreachable_pub
)]
#![warn(
    missing_docs,
    clippy::todo,
    clippy::dbg_macro,
    clippy::clone_on_ref_ptr,
    clippy::future_not_send
)]
#![allow(clippy::missing_docs_in_private_items)]

use chrono::{DateTime, Utc};
use chrono_english::{parse_date_string, Dialect};
use clap::Parser;
use clap_blocks::{
    catalog_dsn::CatalogDsnConfig,
    object_store::{make_object_store, ObjectStoreConfig},
};
use dotenv::dotenv;
use iox_catalog::interface::Catalog;
use object_store::DynObjectStore;
use observability_deps::tracing::*;
use snafu::prelude::*;
use std::{process::ExitCode, sync::Arc};
use tokio::sync::mpsc;
use trogging::{
    cli::LoggingConfig,
    tracing_subscriber::{prelude::*, Registry},
    TroggingGuard,
};

mod checker;
mod deleter;
mod lister;

const BATCH_SIZE: usize = 1000;

fn main() -> ExitCode {
    // load all environment variables from .env before doing anything
    load_dotenv();
    let args = Args::parse();
    let _tracing_guard = init_logs_and_tracing(&args.logging_config);

    if let Err(e) = inner_main(args) {
        use snafu::ErrorCompat;

        eprintln!("{e}");

        for cause in ErrorCompat::iter_chain(&e).skip(1) {
            eprintln!("Caused by: {cause}");
        }

        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

#[tokio::main(flavor = "current_thread")]
async fn inner_main(args: Args) -> Result<()> {
    let object_store = args.object_store()?;
    let catalog = args.catalog().await?;

    let dry_run = args.dry_run;
    let cutoff = args.cutoff()?;
    info!(
        cutoff_arg = %args.cutoff,
        cutoff_parsed = %cutoff,
    );

    let (tx1, rx1) = mpsc::channel(BATCH_SIZE);
    let (tx2, rx2) = mpsc::channel(BATCH_SIZE);

    let lister = tokio::spawn(lister::perform(Arc::clone(&object_store), tx1));
    let checker = tokio::spawn(checker::perform(catalog, cutoff, rx1, tx2));
    let deleter = tokio::spawn(deleter::perform(object_store, dry_run, rx2));

    let (lister, checker, deleter) = futures::join!(lister, checker, deleter);

    deleter.context(DeleterPanicSnafu)??;
    checker.context(CheckerPanicSnafu)??;
    lister.context(ListerPanicSnafu)??;

    Ok(())
}

/// Command-line arguments
#[derive(Debug, Parser)]
pub struct Args {
    #[clap(flatten)]
    object_store: ObjectStoreConfig,

    #[clap(flatten)]
    catalog_dsn: CatalogDsnConfig,

    /// logging options
    #[clap(flatten)]
    pub(crate) logging_config: LoggingConfig,

    /// If this flag is specified, don't delete the files in object storage. Only print the files
    /// that would be deleted if this flag wasn't specified.
    #[clap(long)]
    dry_run: bool,

    /// Items in the object store that are older than this timestamp and also unreferenced in the
    /// catalog will be deleted.
    ///
    /// Can be an exact datetime like `2020-01-01T01:23:45-05:00` or a fuzzy
    /// specification like `1 hour ago`. If not specified, defaults to 14 days ago.
    #[clap(long, default_value_t = String::from("14 days ago"))]
    cutoff: String,
}

impl Args {
    fn object_store(&self) -> Result<Arc<DynObjectStore>> {
        make_object_store(&self.object_store).context(CreatingObjectStoreSnafu)
    }

    async fn catalog(&self) -> Result<Arc<dyn Catalog>> {
        let metrics = metric::Registry::default().into();

        self.catalog_dsn
            .get_catalog("iox_objectstore_garbage_collect", metrics)
            .await
            .context(CreatingCatalogSnafu)
    }

    fn cutoff(&self) -> Result<DateTime<Utc>> {
        let argument = &self.cutoff;
        parse_date_string(argument, Utc::now(), Dialect::Us)
            .context(ParsingCutoffSnafu { argument })
    }
}

#[derive(Debug, Snafu)]
enum Error {
    #[snafu(display("Could not create the object store"))]
    CreatingObjectStore {
        source: clap_blocks::object_store::ParseError,
    },

    #[snafu(display("Could not create the catalog"))]
    CreatingCatalog {
        source: clap_blocks::catalog_dsn::Error,
    },

    #[snafu(display(r#"Could not parse the cutoff "{argument}""#))]
    ParsingCutoff {
        source: chrono_english::DateError,
        argument: String,
    },

    #[snafu(display("The lister task failed"))]
    #[snafu(context(false))]
    Lister { source: lister::Error },
    #[snafu(display("The lister task panicked"))]
    ListerPanic { source: tokio::task::JoinError },

    #[snafu(display("The checker task failed"))]
    #[snafu(context(false))]
    Checker { source: checker::Error },
    #[snafu(display("The checker task panicked"))]
    CheckerPanic { source: tokio::task::JoinError },

    #[snafu(display("The deleter task failed"))]
    #[snafu(context(false))]
    Deleter { source: deleter::Error },
    #[snafu(display("The deleter task panicked"))]
    DeleterPanic { source: tokio::task::JoinError },
}

type Result<T, E = Error> = std::result::Result<T, E>;

fn load_dotenv() {
    match dotenv() {
        Ok(_) => {}
        Err(dotenv::Error::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            // Ignore this - a missing env file is not an error, defaults will
            // be applied when initialising the Config struct.
        }
        Err(e) => {
            eprintln!("FATAL Error loading config from: {}", e);
            eprintln!("Aborting");
            std::process::exit(1);
        }
    };
}

fn init_logs_and_tracing(logging_config: &LoggingConfig) -> TroggingGuard {
    let log_layer = logging_config.to_builder().build().unwrap();
    let subscriber = Registry::default().with(log_layer);
    subscriber::set_global_default(subscriber).unwrap();
    TroggingGuard
}

#[cfg(test)]
mod tests {
    use filetime::FileTime;
    use std::{fs, path::PathBuf};
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn deletes_untracked_files_older_than_the_cutoff() {
        let setup = OldFileSetup::new();

        #[rustfmt::skip]
        let args = Args::parse_from([
            "dummy-program-name",
            "--object-store", "file",
            "--data-dir", setup.data_dir_arg(),
            "--catalog", "memory",
        ]);
        inner_main(args).unwrap();

        assert!(
            !setup.file_path.exists(),
            "The path {} should have been deleted",
            setup.file_path.as_path().display(),
        );
    }

    #[test]
    fn preserves_untracked_files_newer_than_the_cutoff() {
        let setup = OldFileSetup::new();

        #[rustfmt::skip]
        let args = Args::parse_from([
            "dummy-program-name",
            "--object-store", "file",
            "--data-dir", setup.data_dir_arg(),
            "--catalog", "memory",
            "--cutoff", "10 years ago",
        ]);
        inner_main(args).unwrap();

        assert!(
            setup.file_path.exists(),
            "The path {} should not have been deleted",
            setup.file_path.as_path().display(),
        );
    }

    struct OldFileSetup {
        data_dir: TempDir,
        file_path: PathBuf,
    }

    impl OldFileSetup {
        const APRIL_9_2018: FileTime = FileTime::from_unix_time(1523308536, 0);

        fn new() -> Self {
            let data_dir = TempDir::new().unwrap();

            let file_path = data_dir.path().join("some-old-file");
            fs::write(&file_path, "dummy content").unwrap();
            filetime::set_file_mtime(&file_path, Self::APRIL_9_2018).unwrap();

            Self {
                data_dir,
                file_path,
            }
        }

        fn data_dir_arg(&self) -> &str {
            self.data_dir.path().to_str().unwrap()
        }
    }
}
