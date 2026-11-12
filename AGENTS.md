# AGENTS.md

Lee esto antes de modificar nada.

## Qué es

App de escritorio en Rust + Slint que controla la reproducción de otro equipo.
La conexión entre equipos es WebRTC directa, **sin TURN ni relay**; el servidor
solo hace de señalización (axum + tokio). Sin TURN, symmetric/CGNAT pueden no
conectar: es una limitación aceptada, no un bug.

Workspace de Cargo con tres crates:

- `desktop/` — la app.
- `protocol/` — el sobre de señalización (solo serde), compartido por el
  escritorio y el servidor.
- `server/` — el servidor de señalización. Tiene `lib.rs` para levantarlo en
  las pruebas y `main.rs` que solo lo escucha.

`android/` está previsto, sin crear hasta que haya código que lo llene.
El `Cargo.lock` vive en la raíz. El escritorio tiene al servidor como
**dev-dependency**: solo lo usan las pruebas de punta a punta; el binario
publicado no lo lleva.

## Layout de `desktop/`

- `domain/` — tipos y reglas puras, sin infraestructura.
- `ports/` — traits que definen lo que la app necesita.
- `services/` — casos de uso que orquestan los puertos.
- `adapters/` — implementaciones (Slint, WebRTC, medios).
- `platform/` — backends de medios, ventana y red por SO.
- `app.rs` — composición: construye y conecta todo.

## Estado

Fases 0-2 hechas: interfaz por ruta + varios STUN, un socket por interfaz con
IPv6 y re-descubrimiento, y reconexión con ICE restart. Lo que sigue y el
detalle están en `BACKLOG.md`.

## Reglas

1. **Eventos, nunca polling.** El backend se suscribe a los eventos del sistema
   y solo trabaja cuando alguno se dispara. No agregues timers ni bucles que
   consulten el estado periódicamente. Al integrar una API nueva de otro SO,
   buscá su mecanismo de eventos y coalescéalos.
   - Windows (GSMTC): los manejadores solo avisan por canal; el trabajo real
     ocurre en el hilo "gsmtc", con `crossbeam_channel::select!`.
   - Linux (MPRIS): `ProgressTracker::tick()` espera una señal de D-Bus y
     recién entonces relee; el despertar cada 100 ms no consulta el bus.
   - WebRTC: el bucle de `str0m` se guía por `poll_output`; la lectura del
     socket tiene un timeout corto para que la UI no tarde.
   - Cambios de interfaz: `platform::spawn_network_watch` escucha netlink
     (Linux) o `NotifyIpInterfaceChange` (Windows), coalesce la ráfaga y
     avisa; el hilo de pares **solo suma** enlaces nuevos, no cierra los
     viejos.
   - Reintento de ICE: es un plazo por sesión (`RetryPlan`), no una consulta.
2. `#[cfg(...)]` solo dentro de `desktop/src/platform/`, con un único punto de
   despacho en `platform/mod.rs`. Única excepción: el `#![cfg_attr(...)]` de
   `main.rs` que controla la consola en release; los atributos de crate no se
   pueden mover.
3. La dependencia va hacia adentro: `domain/` no depende de nadie; `ports/`
   solo de `domain/`; `services/` de `domain/` y `ports/`; `adapters/`
   implementa los puertos y depende de `domain`, `ports` y la infraestructura.
   `app.rs` es el único que conoce las dos caras.
   El contrato de medios es `domain/media.rs`; el `Sink` compartido por los
   backends vive en `platform/mod.rs`. Los estados propios de un SO se traducen
   en su backend, no en el contrato.
   La red (WebRTC, WebSocket, STUN) es infraestructura y vive en
   `adapters/net/backend/`, fuera de `platform/`; sus tipos raw se traducen a
   `domain::net` en el adaptador de red.
4. `platform/stub.rs` es el **fallback para plataformas sin backend real**
   (macOS, por ahora). No es una implementación de ninguna: no lo tomes como
   referencia.
5. **Identificadores en inglés**, comentarios y doc comments en español.
6. El workspace tiene tres crates. No extraigas más sin un motivo concreto:
   `cargo check` tarda ~10 s, o una dependencia pesada que solo necesita un
   feature. `protocol/` existe porque el escritorio no puede depender de axum
   (compilaría el framework del servidor) y el servidor no puede depender de
   str0m.
7. Las dependencias que solo usa una plataforma van en su bloque
   `[target.'cfg(...)'.dependencies]`, no en `[dependencies]`.
8. Los perfiles de release van en el `Cargo.toml` de la raíz: cargo ignora los
   de los miembros que no son la raíz.
9. **Cada par de WebRTC va dirigido.** `offer`, `answer` e `ice` llevan `to`
   (el sobre está en la versión 2) y el servidor los reenvía solo a ese equipo;
   el aviso de mudanza sigue yendo a toda la sala. **Solo el controlado
   ofrece**: un controlador únicamente contesta ofertas, así dos controladores
   nunca abren un canal entre sí. El controlador aprende quién es su controlado
   de la oferta que recibe, no del orden de los identificadores.
10. **La reconexión la inicia el controlado.** Si el ICE se corta, el panel
    pasa a «Reconectando…» y solo el controlado reinicia con
    `SdpApi::ice_restart` (conservando candidatos); el controlador espera y
    contesta. El plan es 2 s de margen, luego 5/10/20/30 s y 5 s de gracia; si
    no vuelve, pasa a «Sin conexión» y deja de reintentar.

## Trampas

- `slint::include_modules!()` tiene que quedarse en la raíz del crate
  (`main.rs`), porque `platform/windows/window.rs` usa `crate::AppWindow`.
- **En Wayland no se puede mostrar una ventana Slint después de crearla.**
  `winit::set_visible()` es un no-op: la ventana solo se mapea si se creó con
  `visible: true`. Un `show()` desde un callback no la hace aparecer y un
  `hide()` la destruye. Por eso la app tiene **una sola ventana**: los tres
  paneles comparten tamaño y se cambian con `active-panel`, sin redimensionar.
  Si hiciera falta una segunda ventana, hay que crearla y mostrarla antes de
  que arranque el event loop.
- En Windows la ventana se ancla a mano al arrancar (`platform::configure`).
  Como los paneles miden lo mismo, cambiar de panel no redimensiona ni obliga a
  volver a anclarla.
- Desde Slint 1.18 los widgets estándar (`LineEdit`, `ListView`…) hay que
  importarlos (`import { LineEdit } from "std-widgets.slint";`). La ventana de
  red usa un `TextInput` a pelo para no depender del estilo de cada plataforma.
- **Cada plataforma se compila aparte.** `cargo build` solo revisa la del host.
  Para tocar `platform/windows/` hay que pasar
  `--target x86_64-pc-windows-msvc`; `platform/linux/` se compila en Linux.
- **`str0m` con `rust-crypto` igual compila C.** La criptografía es de Rust
  puro, pero `rcgen` arrastra `aws-lc-sys`. No necesita cmake ni NASM, pero sí
  un toolchain de C.
- **WebRTC abre un socket por interfaz usable y anuncia host + srflx de cada
  una.** Se descartan loopback, unspecified y link-local; IPv4 e IPv6 valen
  igual e ICE elige. El agente ICE descarta los paquetes STUN que llegan a una
  dirección que no sea exactamente uno de sus candidatos, así que cada socket
  va atado a la dirección de red (no a `0.0.0.0`). El srflx no tiene socket
  propio: sus paquetes vuelven por el de la interfaz de su base.
  `SINC_NET_ADDR` fija una sola dirección y saltea el barrido. Solo STUN, sin
  TURN: a través de un NAT simétrico no hay camino.
- **El STUN se parsea a mano.** El codec de `str0m::ice` (el crate `is`) exige
  MESSAGE-INTEGRITY y USERNAME, que un STUN público no espera. El request y la
  respuesta del binding (RFC 5389) se arman y parsean en
  `adapters/net/backend/peer.rs`, y se retransmiten desde `maintain_stun`, no
  desde str0m. Se consultan varios servidores desde el mismo socket, cada uno
  con su transacción; el parser acepta relleno de 4 bytes y tanto
  `XOR-MAPPED-ADDRESS` como `MAPPED-ADDRESS`.
- **En Wayland el compositor manda.** `always-on-top`, `set_skip_taskbar` y el
  "no robar foco" no se pueden pedir desde la app; van en la config de
  Hyprland (match por `title`, no por `class`, porque Slint no setea `app_id`).
  `no_focus = true` deja la ventana inenfocable; para no robar foco al aparecer
  va `focus_on_activate = false`. Por eso `platform/linux/window.rs` es un
  no-op.
- **`Rtc::disconnect()` cierra el `Rtc` para siempre**: no sirve para simular
  una caída recuperable. El e2e usa `PeerCommand::SimulateDrop`
  (`#[cfg(test)]`) y deja que el reintento real haga la oferta.

## Verificar

Desde la raíz del workspace:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo xwin clippy --workspace --target x86_64-pc-windows-msvc --all-targets -- -D warnings
cargo test --workspace
```

El **tercer comando es el que importa**: sin `--target`, clippy no ve el código
de Windows, que no compila en Linux. En Linux, el `cargo clippy` a secas es el
que ve `platform/linux/`. El de Windows va con `cargo xwin` porque `str0m`
arrastra `aws-lc-sys`, que es C, y necesita el compilador y las bibliotecas del
SDK de Windows que prepara `cargo-xwin`.

## Correr

Servidor de señalización:

```sh
cargo run -p server
# por defecto escucha en 127.0.0.1:8787; para la VPS:
SINC_SIGNAL_ADDR=0.0.0.0:8787 cargo run -p server
```

En la VPS va detrás de Caddy (o nginx), que termina el TLS: el servidor habla
HTTP plano.

Escritorio:

```sh
cargo run -p sincronin-controlin
# contra otro servidor:
SINC_SIGNAL_URL=wss://senal.example.com/ws cargo run -p sincronin-controlin
# fijar una sola dirección UDP y saltear el barrido de interfaces:
SINC_NET_ADDR=127.0.0.1 cargo run -p sincronin-controlin
# servidores STUN separados por comas; vacío = solo host, sin cruzar NAT:
SINC_STUN_ADDR=stun.l.google.com:19302,stun.cloudflare.com:3478 cargo run -p sincronin-controlin
```

Sin `SINC_STUN_ADDR` se consultan `stun.l.google.com` y `stun.cloudflare.com`.
Por defecto se abre un socket por cada interfaz usable, con la de la ruta de
salida primero.

### Emparejamiento

1. El ícono de cadena abre el panel; al abrirlo, el cliente entra solo en su
   propia sala y muestra el código.
2. El otro equipo escribe ese código en "Sala del otro" y aprieta Enter (o
   "Conectar"): sale de su sala y entra en la del primero.
3. Quedan emparejados y se abre el canal de datos.

El código de sala es aleatorio **por proceso**: si la app se reinicia, cambia.
El controlado es el que entró primero; solo él ofrece.

## Compilar

```sh
cargo xwin build --release --target x86_64-pc-windows-msvc -p sincronin-controlin
# -> target/x86_64-pc-windows-msvc/release/sincronin-controlin.exe
```

En Windows, `SINC_ACTIVATABLE=1` desactiva el «no robar foco» de la ventana,
útil para depurar su comportamiento.
