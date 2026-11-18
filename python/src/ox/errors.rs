// Vendored from pyoxigraph 0.5.13 (https://github.com/oxigraph/oxigraph/tree/main/python),
// MIT OR Apache-2.0, Copyright (c) Oxigraph contributors. Classes registered in the `tripletrail`
// module instead of `pyoxigraph`; see python/src/ox/README.md.

use crate::ox::io::map_parse_error;
use oxigraph::store::{LoaderError, SerializerError, StorageError};
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use std::path::PathBuf;

pub fn map_storage_error(error: StorageError) -> PyErr {
    match error {
        StorageError::Io(error) => error.into(),
        _ => PyRuntimeError::new_err(error.to_string()),
    }
}

pub fn map_loader_error(error: LoaderError, file_path: Option<PathBuf>) -> PyErr {
    match error {
        LoaderError::Storage(error) => map_storage_error(error),
        LoaderError::Parsing(error) => map_parse_error(error, file_path),
        LoaderError::InvalidBaseIri { .. } => PyValueError::new_err(error.to_string()),
    }
}

pub fn map_serializer_error(error: SerializerError) -> PyErr {
    match error {
        SerializerError::Storage(error) => map_storage_error(error),
        SerializerError::Io(error) => error.into(),
        SerializerError::DatasetFormatExpected(_) => PyValueError::new_err(error.to_string()),
    }
}
