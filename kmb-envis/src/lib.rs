pub mod append;
pub mod config;
pub mod db;
pub mod definition;
pub mod emi;
pub mod pack;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database: {0}")]
    Sql(String),
    #[error("not a supported ENVIS database: {0}")]
    Version(String),
    #[error("archive {0}: no ENVIS definition with this record layout is stored")]
    NoDefinition(u8),
    #[error("row of {got} bytes, the definition has {expected}")]
    RowLength { expected: usize, got: usize },
    #[error("time: {0}")]
    Time(String),
    #[error(transparent)]
    Definition(#[from] definition::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
