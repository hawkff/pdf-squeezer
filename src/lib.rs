//! pdf-squeezer compresses PDF files and converts them to PDF/A.

pub mod document;
pub mod error;
pub mod extract;
pub mod files;
pub mod filters;
pub mod image_encoding;
pub mod images;
pub mod options;
pub mod pdf;
pub mod process;
pub mod profiles;
pub mod run;
pub mod tools;

pub use run::run;
