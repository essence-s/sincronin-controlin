//! Servicios de la capa de aplicación (casos de uso).
//!
//! [`state`] y [`pairing`] orquestan los puertos sin conocer adaptadores
//! concretos. [`controller`] implementa el puerto de entrada y traduce las
//! acciones de la UI a esos casos de uso. Todo se instancia y se conecta en
//! [`crate::app`].

pub mod controller;
pub mod pairing;
pub mod state;
