//! Adaptador de UI: implementa [`crate::ports::UiPort`] sobre Slint.
//!
//! El cableado de los callbacks vive en [`crate::app`]; este módulo solo
//! pinta el estado que le piden los servicios.

pub mod slint;
