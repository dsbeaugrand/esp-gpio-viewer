//! Hand-rolled minimal async HTTP/1.1 + SSE server over embassy-net (feature `server`).
//!
//! Replaces picoserve with a shallow, allocation-free serve path: read the request line, route to
//! a [`crate::protocol`] serializer, and write the byte-exact body. `/events` streams the sampler's
//! broadcast [`Frame`]s as Server-Sent-Events. The poll depth is small and constant (no giant
//! router future, no embassy-time timeout machinery deep in the poll), which dissolves the
//! stack/DRAM pressure picoserve's serve future caused.
//!
//! The pure request/response string helpers live in [`crate::http`] (compiled unconditionally and
//! host-tested); this module holds only the embassy-net I/O, gated behind `server`.
//!
//! ## Serving model
//! One response per connection (`Connection: close`), except `/events`, which is a long-lived SSE
//! stream. Concurrency comes from running several independent [`accept_loop`] tasks, each with its
//! own [`TcpSocket`] + buffers — so the persistent `/events` stream on one socket never blocks the
//! hosted UI's parallel REST fetches on another. Every socket write is guarded; a client that drops
//! mid-write ends the handler cleanly (never panics).

use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_time::Duration;
use embedded_io_async::Write as _;
use heapless::{String, Vec as HeaplessVec};

use crate::http::{parse_request_target, response_header};
use crate::sampler::{self, Frame, FrameChannel, PinChange};
use crate::{capabilities, hwinfo, protocol, reader, GpioViewer, MAX_REGISTERED_PINS};

/// Maximum length of the dotted-quad IP string injected into the index page.
/// `255.255.255.255` is 15 bytes; the buffer matches that worst case.
pub const IP_STR_CAP: usize = 15;

/// Maximum length of the pre-formatted free-sketch-RAM string (`formatBytes` output,
/// e.g. `"1.20 MB"`). 16 bytes covers every realistic value with headroom.
pub const FREE_SKETCH_RAM_CAP: usize = 16;

/// Bytes buffered from a connection while scanning for the end of the request headers. A GET
/// request line is short; this comfortably holds it plus a browser's header block. If the block is
/// longer, we still have the request line (it comes first) and route on it.
const REQUEST_BUF: usize = 1024;

/// Idle timeout applied to each served socket. A client that stalls mid-request (or never sends a
/// request line) releases its socket after this, so the small socket pool cannot be starved.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// A representative ESP32 partition table, used as the `/partition` fallback when firmware injects
/// no real table. DATA partitions are listed before APP, matching the C++ ordering
/// (`gpio_viewer.h:318`). Firmware should prefer the real table read via `esp-bootloader-esp-idf`
/// (see the per-chip examples).
pub const DEFAULT_PARTITIONS: &[protocol::PartitionInfo<'static>] = &[
    protocol::PartitionInfo {
        label: "nvs",
        ptype: 1,
        subtype: 2,
        address: 0x9000,
        size: 0x5000,
    },
    protocol::PartitionInfo {
        label: "otadata",
        ptype: 1,
        subtype: 0,
        address: 0xE000,
        size: 0x2000,
    },
    protocol::PartitionInfo {
        label: "app0",
        ptype: 0,
        subtype: 16,
        address: 0x10000,
        size: 0x140000,
    },
];

/// Shared, read-only server state handed by reference to every connection.
///
/// Unlike the former picoserve `AppState`, this needs no `Clone`/`State` machinery — the handler
/// just borrows it, so there is no per-request deep clone. It carries the configured [`GpioViewer`]
/// (by `'static` reference), the two runtime strings the index page needs (IP + free-sketch-RAM),
/// the sampler broadcast channel, and the flash partition table.
pub struct ServerState {
    /// The configured viewer (release string, sampling interval, port, pin registry, seams).
    pub viewer: &'static GpioViewer,
    /// Device IP as a dotted-quad string, injected into the index page's `window.gpio_settings.ip`.
    pub ip: String<IP_STR_CAP>,
    /// Pre-formatted free-sketch-RAM string, injected into `window.gpio_settings.freeSketchRam`.
    pub free_sketch_ram: String<FREE_SKETCH_RAM_CAP>,
    /// Broadcast channel the sampler publishes frames to; each `/events` client subscribes.
    pub events: &'static FrameChannel,
    /// The flash partition table served at `/partition` (or [`DEFAULT_PARTITIONS`]).
    pub partitions: &'static [protocol::PartitionInfo<'static>],
}

impl ServerState {
    /// Build the state. Both string inputs are truncated to their buffer capacities rather than
    /// panicking, matching the graceful-overflow style of the protocol serializers.
    pub fn new(
        viewer: &'static GpioViewer,
        ip: &str,
        free_sketch_ram: &str,
        events: &'static FrameChannel,
        partitions: &'static [protocol::PartitionInfo<'static>],
    ) -> Self {
        let mut ip_buffer: String<IP_STR_CAP> = String::new();
        let _ = ip_buffer.push_str(ip);
        let mut ram_buffer: String<FREE_SKETCH_RAM_CAP> = String::new();
        let _ = ram_buffer.push_str(free_sketch_ram);
        ServerState {
            viewer,
            ip: ip_buffer,
            free_sketch_ram: ram_buffer,
            events,
            partitions,
        }
    }
}

/// Serve one TCP socket forever: accept a connection on `port`, handle it, close, repeat. Never
/// returns.
///
/// `rx_buffer`/`tx_buffer` back the socket and are reused across connections. Spawn several of
/// these (each with its own buffers) for concurrency, so the long-lived `/events` stream on one
/// socket does not block the UI's REST fetches on another.
pub async fn accept_loop(
    stack: Stack<'static>,
    port: u16,
    state: &ServerState,
    rx_buffer: &mut [u8],
    tx_buffer: &mut [u8],
) -> ! {
    loop {
        // Reborrow the buffers each iteration; the previous socket has been dropped by now.
        let mut socket = TcpSocket::new(stack, &mut *rx_buffer, &mut *tx_buffer);
        socket.set_timeout(Some(CONNECTION_TIMEOUT));

        if socket.accept(port).await.is_err() {
            // Listen/accept failed (e.g. a transient state error); reset and retry.
            socket.abort();
            continue;
        }

        serve_connection(&mut socket, state).await;

        // Half-close (send FIN). The full response was already flushed, so `Connection: close`
        // clients (which read to Content-Length) already have their bytes.
        socket.close();

        // Linger until the peer ACKs our data and sends its own FIN before we drop the socket.
        // Dropping a not-fully-closed `TcpSocket` makes embassy-net abort it (RST), which under
        // packet loss can race and truncate the response tail — some clients then surface a reset
        // even on a complete body. Draining to the peer's FIN turns that into a clean close. The
        // socket's `CONNECTION_TIMEOUT` bounds this, so a peer that never closes cannot wedge the
        // loop. Then recreating the socket next iteration reclaims the buffers.
        let mut drain = [0u8; 64];
        while let Ok(read_bytes) = socket.read(&mut drain).await {
            if read_bytes == 0 {
                break; // peer's FIN observed
            }
        }
    }
}

/// Handle a single accepted connection: read the request line, route, respond. Never panics —
/// any socket error simply ends the handler.
async fn serve_connection(socket: &mut TcpSocket<'_>, state: &ServerState) {
    let mut request_buf = [0u8; REQUEST_BUF];
    let filled = match read_request_head(socket, &mut request_buf).await {
        Ok(filled) => filled,
        Err(()) => return, // socket error / EOF before any request line
    };
    // Request lines are ASCII; a non-UTF-8 request is malformed and routes to 404.
    let request = core::str::from_utf8(&request_buf[..filled]).unwrap_or("");
    let path = parse_request_target(request);

    // `/events` is a long-lived SSE stream; everything else is one fixed-length response.
    if path == Some("/events") {
        let _ = serve_sse(socket, state).await;
    } else {
        let _ = serve_rest(socket, state, path).await;
    }
}

/// Route a non-SSE request to its [`crate::protocol`] serializer and write the response. `Err(())`
/// means the client dropped mid-write (the caller just ends the connection).
async fn serve_rest(
    socket: &mut TcpSocket<'_>,
    state: &ServerState,
    path: Option<&str>,
) -> Result<(), ()> {
    let viewer = state.viewer;
    match path {
        Some("/") => {
            let body = protocol::index_html(
                state.ip.as_str(),
                viewer.port,
                state.free_sketch_ram.as_str(),
            );
            write_response(socket, "200 OK", "text/html", body.as_bytes()).await
        }
        Some("/release") => {
            let body = protocol::release_body(viewer.release.as_str());
            write_response(socket, "200 OK", "application/json", body.as_bytes()).await
        }
        Some("/sampling") => {
            let body = protocol::sampling_body(viewer.sampling_interval_ms);
            write_response(socket, "200 OK", "application/json", body.as_bytes()).await
        }
        Some("/free_psram") => {
            let free_psram = sampler::resolve_free_psram(viewer.free_psram_source);
            let body = protocol::free_psram_body(viewer.sampling_interval_ms, free_psram);
            write_response(socket, "200 OK", "application/json", body.as_bytes()).await
        }
        Some("/pinmodes") => {
            let pairs = viewer.pinmode_pairs();
            let body = protocol::pinmodes_body(pairs.as_slice());
            write_response(socket, "200 OK", "application/json", body.as_bytes()).await
        }
        Some("/pinfunctions") => {
            let body =
                protocol::pinfunctions_body(capabilities::ADC_PINS, capabilities::TOUCH_PINS);
            write_response(socket, "200 OK", "application/json", body.as_bytes()).await
        }
        Some("/espinfo") => {
            let free_heap = sampler::resolve_free_heap(viewer.free_heap_source);
            let free_psram = sampler::resolve_free_psram(viewer.free_psram_source);
            let info = hwinfo::espinfo(free_heap, free_psram);
            let body = protocol::espinfo_body(&info);
            write_response(socket, "200 OK", "application/json", body.as_bytes()).await
        }
        Some("/partition") => {
            let body = protocol::partition_body(state.partitions);
            write_response(socket, "200 OK", "application/json", body.as_bytes()).await
        }
        _ => write_response(socket, "404 Not Found", "text/plain", b"Not Found").await,
    }
}

/// `GET /events` — the Server-Sent-Events stream feeding the hosted UI's live tiles.
///
/// Sends the SSE headers, a connect-time baseline `gpio-state` covering every registered pin
/// (`resetStatePins` semantics), then forwards each broadcast [`Frame`] until the client
/// disconnects (a write error).
async fn serve_sse(socket: &mut TcpSocket<'_>, state: &ServerState) -> Result<(), ()> {
    const SSE_HEADERS: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: text/event-stream\r\n\
Cache-Control: no-cache\r\n\
Connection: keep-alive\r\n\r\n";
    socket.write_all(SSE_HEADERS).await.map_err(|_| ())?;

    // Baseline: read every registered pin now and emit one gpio-state frame so the UI renders all
    // tiles immediately, before the first sampler diff arrives.
    let mut baseline: HeaplessVec<PinChange, MAX_REGISTERED_PINS> = HeaplessVec::new();
    for registered in state.viewer.pins.iter() {
        let reading = reader::read_pin(registered, state.viewer.analog_source);
        // Capacity equals the registry cap, so this push cannot overflow.
        let _ = baseline.push((
            registered.pin,
            reading.scaled,
            reading.raw,
            reading.pin_type.to_int(),
        ));
    }
    let baseline_body = protocol::gpio_state_body(&baseline);
    write_sse_event(socket, "gpio-state", baseline_body.as_str()).await?;

    // Subscribe and stream live frames. If every subscriber slot is taken we still served the
    // baseline; close rather than busy-hold the socket.
    let Ok(mut subscriber) = state.events.subscriber() else {
        return Ok(());
    };
    loop {
        // `next_message_pure` transparently skips `Lagged` markers from a slow reader.
        let frame = subscriber.next_message_pure().await;
        let (name, data) = match &frame {
            Frame::GpioState(body) => ("gpio-state", body.as_str()),
            Frame::FreeHeap(body) => ("free_heap", body.as_str()),
            Frame::FreePsram(body) => ("free_psram", body.as_str()),
        };
        write_sse_event(socket, name, data).await?;
    }
}

/// Write a complete fixed-length HTTP response (status line + headers + body), then flush.
async fn write_response(
    socket: &mut TcpSocket<'_>,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), ()> {
    let header = response_header(status, content_type, body.len());
    socket.write_all(header.as_bytes()).await.map_err(|_| ())?;
    socket.write_all(body).await.map_err(|_| ())?;
    socket.flush().await.map_err(|_| ())?;
    Ok(())
}

/// Write one SSE event (`event: <name>\ndata: <data>\n\n`) and flush so the client sees it
/// immediately.
async fn write_sse_event(socket: &mut TcpSocket<'_>, name: &str, data: &str) -> Result<(), ()> {
    socket.write_all(b"event: ").await.map_err(|_| ())?;
    socket.write_all(name.as_bytes()).await.map_err(|_| ())?;
    socket.write_all(b"\ndata: ").await.map_err(|_| ())?;
    socket.write_all(data.as_bytes()).await.map_err(|_| ())?;
    socket.write_all(b"\n\n").await.map_err(|_| ())?;
    socket.flush().await.map_err(|_| ())?;
    Ok(())
}

/// Read from `socket` into `buf` until the end-of-headers marker (`\r\n\r\n`) is seen or `buf` is
/// full. Returns the number of bytes buffered, or `Err(())` on a socket error or an EOF before any
/// bytes arrived. A full buffer or a partial read still returns whatever was captured — the request
/// line comes first, so routing works even if the header block was truncated.
async fn read_request_head(socket: &mut TcpSocket<'_>, buf: &mut [u8]) -> Result<usize, ()> {
    let mut filled = 0usize;
    loop {
        if filled == buf.len() {
            return Ok(filled);
        }
        match socket.read(&mut buf[filled..]).await {
            Ok(0) => return if filled > 0 { Ok(filled) } else { Err(()) },
            Ok(n) => {
                filled += n;
                if contains_subslice(&buf[..filled], b"\r\n\r\n") {
                    return Ok(filled);
                }
            }
            Err(_) => return Err(()),
        }
    }
}

/// Whether `haystack` contains `needle` as a contiguous subslice.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}
