//! El binario del servidor de señalización.
//!
//! Toda la lógica está en la biblioteca (`server`). Acá solo se levanta el
//! servidor y se escucha.
//!
//! ## Como se corre
//!
//! ```sh
//! cargo run -p server
//! # o, para que escuche en todas las interfaces:
//! SINC_SIGNAL_ADDR=0.0.0.0:8787 cargo run -p server
//! ```
//!
//! **TLS**: en esta versión el servidor escucha en HTTP plano. En la VPS
//! va detras de Caddy (o nginx), que termina el TLS y le pasa el
//! WebSocket por HTTP plano.

use std::sync::Arc;

use tokio::net::TcpListener;

use server::{State, serve};

/// Dirección de escucha por defecto.
const DEFAULT_ADDR: &str = "127.0.0.1:8787";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = std::env::var("SINC_SIGNAL_ADDR").unwrap_or_else(|_| DEFAULT_ADDR.to_string());

    let state = Arc::new(State::new());
    let listener = TcpListener::bind(&addr).await?;

    eprintln!("[señal] escuchando en {addr} (ws://{addr}/ws?room=CODIGO)");

    serve(listener, state).await?;

    Ok(())
}
