//! Wayland cursor overlay for UI-cursor mode inside nested Gamescope.
//!
//! Creates a transparent, fullscreen `zwlr_layer_shell_v1` overlay surface on
//! the Gamescope nested compositor. The overlay carries no keyboard
//! interactivity and toggles between two input states:
//!
//! * **enabled (UI mode):** `set_input_region(None)` → infinite input region.
//!   The overlay receives `wl_pointer` motion/button events with absolute,
//!   surface-relative coordinates. Those are emitted as [`CursorEvent`]s and
//!   translated by the game session into Android touchscreen events.
//! * **disabled (aim / OS-release):** an empty `wl_region` is installed as the
//!   input region, so the overlay is input-invisible and every pointer event
//!   passes through to the game window below.
//!
//! The overlay never injects input itself; it only reports cursor events to
//! the main session loop through the `emit` callback.

use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_pointer, wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool,
    wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

/// Linux input event code for the left mouse button (`BTN_LEFT`).
const BTN_LEFT: u32 = 0x110;
/// Timeout for control commands sent to the overlay thread.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(2);
/// Poll timeout (ms) for the Wayland event loop so commands are noticed.
const POLL_TIMEOUT_MS: i32 = 50;
/// Timeout for the overlay startup handshake.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Classic left-leaning arrow cursor sprite, `X` = black outline, `o` = white
/// fill, `.` = transparent. Blitted directly into the overlay surface's
/// full-surface buffers so UI cursor mode keeps a visible pointer even when
/// the host compositor hides its sprite.
const CURSOR_ROWS: &[&str] = &[
    "X...........",
    "XX..........",
    "XoX.........",
    "XooX........",
    "XoooX.......",
    "XooooX......",
    "XoooooX.....",
    "XooooooX....",
    "XoooooooX...",
    "XooooooooX..",
    "XoooooooooX.",
    "XooooXXXXXX.",
    "XooXoX......",
    "XoXX.XoX....",
    "XX...XoX....",
    "X.....XoX...",
    "......XX....",
];
const CURSOR_WIDTH: i32 = 12;
const CURSOR_HEIGHT: i32 = CURSOR_ROWS.len() as i32;
const CURSOR_OUTLINE: u32 = 0xFF00_0000;
const CURSOR_FILL: u32 = 0xFFFF_FFFF;
const CURSOR_TRANSPARENT: u32 = 0x0000_0000;

/// Events emitted by the cursor overlay, with absolute surface-relative
/// coordinates and the surface size needed to map them into Android space.
#[derive(Debug, Clone, Copy)]
pub enum CursorEvent {
    /// Pointer moved to `(x, y)` inside a surface of the given size.
    Motion {
        x: f64,
        y: f64,
        surface_width: u32,
        surface_height: u32,
    },
    /// Left mouse button pressed or released at `(x, y)`.
    LeftButton {
        pressed: bool,
        x: f64,
        y: f64,
        surface_width: u32,
        surface_height: u32,
    },
}

/// Commands sent from the session thread to the overlay thread.
enum CursorOverlayCommand {
    SetEnabled {
        enabled: bool,
        reply: Sender<io::Result<()>>,
    },
    /// Move or hide the session-drawn cursor sprite. The virtual cursor is
    /// rendered by the overlay itself because Gamescope's nested (xdg)
    /// backend implements neither host cursor control nor pointer delivery
    /// to nested clients ("NO CURSOR IMPL XDG").
    SetCursor {
        visible: bool,
        x: f64,
        y: f64,
        reply: Sender<io::Result<()>>,
    },
    Stop,
}

/// Handle to the cursor overlay worker thread.
pub struct CursorOverlay {
    control: Sender<CursorOverlayCommand>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl CursorOverlay {
    /// Spawn the overlay thread, connect to the nested Gamescope Wayland
    /// display, and block until the overlay is mapped (or fails).
    pub fn spawn<F>(runtime_dir: &Path, display: &str, enabled: bool, emit: F) -> io::Result<Self>
    where
        F: Fn(CursorEvent) + Send + 'static,
    {
        let (control_tx, control_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let runtime_dir = runtime_dir.to_path_buf();
        let display = display.to_string();
        let emit: Box<dyn Fn(CursorEvent) + Send> = Box::new(emit);

        let thread = thread::Builder::new()
            .name("wroid-cursor-overlay".into())
            .spawn(move || {
                run_overlay(runtime_dir, display, enabled, control_rx, emit, ready_tx)
            })?;

        match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = control_tx.send(CursorOverlayCommand::Stop);
                let _ = thread.join();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "cursor overlay startup timeout",
                ));
            }
        }

        Ok(Self {
            control: control_tx,
            thread: Some(thread),
        })
    }

    /// Enable or disable the overlay input region.
    ///
    /// `true` makes the overlay receive pointer events (UI mode); `false`
    /// makes it input-transparent (aim / OS-release).
    pub fn set_enabled(&self, enabled: bool) -> io::Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.control
            .send(CursorOverlayCommand::SetEnabled {
                enabled,
                reply: reply_tx,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "overlay thread stopped"))?;
        reply_rx
            .recv_timeout(CONTROL_TIMEOUT)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "overlay control timeout"))?
    }

    /// Position the session-drawn cursor sprite, or hide it.
    ///
    /// Coordinates are surface-local pixels of the overlay surface. The
    /// sprite is only drawn while visible; hidden state re-attaches the
    /// transparent 1x1 buffer so the surface stays mapped.
    pub fn set_cursor(&self, visible: bool, x: f64, y: f64) -> io::Result<()> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.control
            .send(CursorOverlayCommand::SetCursor {
                visible,
                x,
                y,
                reply: reply_tx,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "overlay thread stopped"))?;
        reply_rx
            .recv_timeout(CONTROL_TIMEOUT)
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "overlay control timeout"))?
    }
}

impl Drop for CursorOverlay {
    fn drop(&mut self) {
        let _ = self.control.send(CursorOverlayCommand::Stop);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Mutable state shared across the overlay's `Dispatch` implementations.
struct OverlayState {
    enabled: bool,
    configured_size: Option<(u32, u32)>,
    compositor: Option<wl_compositor::WlCompositor>,
    seat: Option<wl_seat::WlSeat>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    surface: Option<wl_surface::WlSurface>,
    layer_surface: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
    pointer: Option<wl_pointer::WlPointer>,
    /// Full-surface sprite buffers; the arrow is blitted into them at the
    /// requested position because `wl_surface.attach` forbids nonzero
    /// offsets at version >= 5, so the sprite cannot move via attach.
    sprite_buffers: [Option<wl_buffer::WlBuffer>; 2],
    /// Surface size the sprite buffers were created for; drawing is skipped
    /// when the compositor reconfigures a different size.
    sprite_buffer_size: Option<(i32, i32)>,
    sprite_stride: i32,
    /// Arrow tip currently blitted into each sprite buffer.
    sprite_drawn: [Option<(i32, i32)>; 2],
    sprite_index: usize,
    sprite_pixels: Option<SpritePixels>,
    pending_cursor_visible: bool,
    pending_cursor_x: i32,
    pending_cursor_y: i32,
    /// Kept alive so the compositor can map the shared-memory pool.
    _cursor_memfd: Option<File>,
    last_x: f64,
    last_y: f64,
    left_pressed: bool,
    emit: Box<dyn Fn(CursorEvent) + Send>,
    ready: Option<Sender<io::Result<()>>>,
    closed: bool,
}

/// Shared-memory mapping of the sprite pool, writable from this thread.
struct SpritePixels {
    pointer: *mut u8,
    length: usize,
}

impl SpritePixels {
    fn map(memfd: &File, length: usize) -> io::Result<Self> {
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                length,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                memfd.as_raw_fd(),
                0,
            )
        };
        if pointer == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            pointer: pointer.cast::<u8>(),
            length,
        })
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.pointer, self.length) }
    }
}

impl Drop for SpritePixels {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.pointer.cast::<libc::c_void>(), self.length);
        }
    }
}

/// Overlay worker entry point: connect, set up the layer surface, then pump
/// events until stopped or the compositor goes away.
fn run_overlay(
    runtime_dir: PathBuf,
    display: String,
    enabled: bool,
    commands: Receiver<CursorOverlayCommand>,
    emit: Box<dyn Fn(CursorEvent) + Send>,
    ready: Sender<io::Result<()>>,
) -> io::Result<()> {
    let socket_path = runtime_dir.join(&display);
    let stream = UnixStream::connect(&socket_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("connect nested display {socket_path:?}: {e}"),
        )
    })?;
    let connection = Connection::from_socket(stream)
        .map_err(|e| io::Error::other(format!("wayland handshake: {e}")))?;

    let mut event_queue: EventQueue<OverlayState> = connection.new_event_queue();
    let mut state = OverlayState {
        enabled,
        configured_size: None,
        compositor: None,
        seat: None,
        shm: None,
        layer_shell: None,
        surface: None,
        layer_surface: None,
        pointer: None,
        sprite_buffers: [None, None],
        sprite_buffer_size: None,
        sprite_stride: 0,
        sprite_drawn: [None, None],
        sprite_index: 0,
        sprite_pixels: None,
        pending_cursor_visible: false,
        pending_cursor_x: 0,
        pending_cursor_y: 0,
        _cursor_memfd: None,
        last_x: 0.0,
        last_y: 0.0,
        left_pressed: false,
        emit,
        ready: Some(ready),
        closed: false,
    };

    // Advertise globals.
    let display_handle = connection.display();
    let _registry = display_handle.get_registry(&event_queue.handle(), ());
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| io::Error::other(format!("registry roundtrip: {e}")))?;

    if state.compositor.is_none()
        || state.layer_shell.is_none()
        || state.shm.is_none()
        || state.seat.is_none()
    {
        let err = io::Error::new(
            io::ErrorKind::NotFound,
            "missing required Wayland globals (wl_compositor / zwlr_layer_shell_v1 / wl_shm / wl_seat)",
        );
        if let Some(r) = state.ready.take() {
            let _ = r.send(Err(err));
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "missing required Wayland globals",
        ));
    }

    // Create the fullscreen overlay surface.
    let compositor = state
        .compositor
        .clone()
        .expect("compositor global verified above");
    let layer_shell = state
        .layer_shell
        .clone()
        .expect("layer_shell global verified above");
    let surface = compositor.create_surface(&event_queue.handle(), ());
    let layer_surface = layer_shell.get_layer_surface(
        &surface,
        None,
        zwlr_layer_shell_v1::Layer::Overlay,
        "wroid-cursor-overlay".to_string(),
        &event_queue.handle(),
        (),
    );
    layer_surface.set_size(0, 0);
    layer_surface.set_anchor(
        zwlr_layer_surface_v1::Anchor::Top
            | zwlr_layer_surface_v1::Anchor::Bottom
            | zwlr_layer_surface_v1::Anchor::Left
            | zwlr_layer_surface_v1::Anchor::Right,
    );
    layer_surface.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);

    surface.commit();
    state.surface = Some(surface);
    state.layer_surface = Some(layer_surface);

    // Wait for the compositor to configure the surface.
    for _ in 0..10 {
        event_queue
            .roundtrip(&mut state)
            .map_err(|e| io::Error::other(format!("configure roundtrip: {e}")))?;
        if state.configured_size.is_some() {
            break;
        }
    }
    if state.configured_size.is_none() {
        if let Some(r) = state.ready.take() {
            let _ = r.send(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "layer surface never configured",
            )));
        }
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "layer surface never configured",
        ));
    }

    // Map the surface with the first (still fully transparent) sprite buffer
    // and prepare both sprite buffers for the session-drawn virtual cursor.
    create_sprite_buffers(&mut state, &event_queue.handle())?;
    let (surface_width, surface_height) = state
        .sprite_buffer_size
        .expect("sprite buffers require a configured size");
    let surface = state
        .surface
        .as_ref()
        .expect("surface created before buffers");
    surface.attach(state.sprite_buffers[0].as_ref(), 0, 0);
    surface.damage(0, 0, surface_width, surface_height);
    surface.commit();
    // Apply the initial input region.
    apply_input_region(&mut state, &event_queue.handle());

    if let Some(r) = state.ready.take() {
        let _ = r.send(Ok(()));
    }

    event_loop(&mut event_queue, &mut state, &commands)
}

/// Create an anonymous memfd of the given size.
fn create_memfd(name: &[u8], size: usize) -> io::Result<File> {
    let fd = unsafe {
        libc::syscall(
            libc::SYS_memfd_create,
            name.as_ptr() as *const libc::c_char,
            libc::MFD_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { File::from_raw_fd(fd as i32) };
    file.set_len(size as u64)?;
    Ok(file)
}

/// Build the cursor sprite buffers: two full-surface ARGB8888 copies sharing
/// one shm pool. Both start fully transparent (fresh memfd pages are
/// zero-filled) and the arrow is blitted into whichever copy is committed
/// next; alternating the two defeats Gamescope's identical-buffer dedupe.
fn create_sprite_buffers(
    state: &mut OverlayState,
    qh: &QueueHandle<OverlayState>,
) -> io::Result<()> {
    let Some(shm) = state.shm.clone() else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "wl_shm not available",
        ));
    };
    let Some((width, height)) = state.configured_size.and_then(|(width, height)| {
        Some((i32::try_from(width).ok()?, i32::try_from(height).ok()?))
    }) else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "layer surface was not configured yet",
        ));
    };

    let stride = width
        .checked_mul(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "stride overflow"))?;
    let frame = stride
        .checked_mul(height)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "buffer size overflow"))?;
    let pool_size = frame
        .checked_mul(2)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pool size overflow"))?;

    let memfd = create_memfd(b"wroid-cursor-sprite\0", pool_size as usize)?;
    let pixels = SpritePixels::map(&memfd, pool_size as usize)?;
    let pool = shm.create_pool(memfd.as_fd(), pool_size, qh, ());
    let first = pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, qh, ());
    let second = pool.create_buffer(
        frame,
        width,
        height,
        stride,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );

    state.sprite_buffers = [Some(first), Some(second)];
    state.sprite_buffer_size = Some((width, height));
    state.sprite_stride = stride;
    state.sprite_drawn = [None, None];
    state.sprite_index = 0;
    state.sprite_pixels = Some(pixels);
    state._cursor_memfd = Some(memfd);
    Ok(())
}

/// Draw (or hide) the virtual cursor sprite on the main overlay surface.
///
/// The arrow is blitted into the next full-surface buffer at the requested
/// position and committed with attach offset `(0, 0)`; `wl_surface.attach`
/// has required zero offsets since version 5, so the sprite moves inside the
/// buffer, not via the attach offset. Alternating buffers keeps every commit
/// distinct for Gamescope's dedupe.
fn draw_cursor(state: &mut OverlayState) {
    let Some(surface) = state.surface.clone() else {
        return;
    };
    let Some((width, height)) = state.sprite_buffer_size else {
        return;
    };
    let matches_configured = match state.configured_size {
        Some((configured_width, configured_height)) => {
            i32::try_from(configured_width) == Ok(width)
                && i32::try_from(configured_height) == Ok(height)
        }
        None => false,
    };
    if !matches_configured {
        // The compositor reconfigured a different size; the pool no longer
        // matches the surface, so skip drawing rather than risk a protocol
        // error.
        return;
    }

    let new_x = state
        .pending_cursor_x
        .clamp(0, width.saturating_sub(CURSOR_WIDTH));
    let new_y = state
        .pending_cursor_y
        .clamp(0, height.saturating_sub(CURSOR_HEIGHT));
    let drawn = state.pending_cursor_visible.then_some((new_x, new_y));

    state.sprite_index ^= 1;
    let index = state.sprite_index;
    let old_rect = state.sprite_drawn[index];
    if old_rect == drawn {
        // This buffer already shows exactly that; nothing to commit.
        return;
    }

    if let Some(pixels) = state.sprite_pixels.as_mut() {
        let bytes = pixels.as_mut_slice();
        clear_sprite_rect(bytes, state.sprite_stride, old_rect);
        if let Some((x, y)) = drawn {
            blit_arrow(bytes, state.sprite_stride, x, y);
        }
    }
    state.sprite_drawn[index] = drawn;

    if let Some(buffer) = state.sprite_buffers[index].as_ref() {
        surface.attach(Some(buffer), 0, 0);
        if let Some((x, y)) = old_rect {
            surface.damage(x, y, CURSOR_WIDTH, CURSOR_HEIGHT);
        }
        if let Some((x, y)) = drawn {
            surface.damage(x, y, CURSOR_WIDTH, CURSOR_HEIGHT);
        }
        surface.commit();
    }
}

/// Erase the arrow at `rect` from the sprite pool bytes.
fn clear_sprite_rect(bytes: &mut [u8], stride: i32, rect: Option<(i32, i32)>) {
    let Some((x, y)) = rect else {
        return;
    };
    for row in 0..CURSOR_HEIGHT {
        let start = ((y + row) * stride + x * 4) as usize;
        let end = start + CURSOR_WIDTH as usize * 4;
        if end <= bytes.len() {
            bytes[start..end].fill(0);
        }
    }
}

/// Blit the arrow bitmap with its tip at `(x, y)` into the sprite pool bytes.
fn blit_arrow(bytes: &mut [u8], stride: i32, x: i32, y: i32) {
    for (row_index, row) in CURSOR_ROWS.iter().enumerate() {
        for column in 0..CURSOR_WIDTH {
            let cell = row
                .chars()
                .nth(column as usize)
                .filter(|c| *c == 'X' || *c == 'o')
                .map(|c| {
                    if c == 'X' {
                        CURSOR_OUTLINE
                    } else {
                        CURSOR_FILL
                    }
                })
                .unwrap_or(CURSOR_TRANSPARENT);
            let start = ((y + row_index as i32) * stride + (x + column) * 4) as usize;
            let end = start + 4;
            if end <= bytes.len() {
                bytes[start..end].copy_from_slice(&cell.to_ne_bytes());
            }
        }
    }
}

/// Install the input region matching the current `enabled` flag and commit.
fn apply_input_region(state: &mut OverlayState, qh: &QueueHandle<OverlayState>) {
    let Some(surface) = state.surface.as_ref() else {
        return;
    };
    if state.enabled {
        // Infinite input region: the overlay receives all pointer events.
        surface.set_input_region(None);
    } else {
        // Empty input region: the overlay is input-transparent.
        let Some(compositor) = state.compositor.as_ref() else {
            return;
        };
        let region = compositor.create_region(qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
    }
    surface.commit();
}

/// Main event pump: interleave command handling with Wayland dispatch.
fn event_loop(
    event_queue: &mut EventQueue<OverlayState>,
    state: &mut OverlayState,
    commands: &Receiver<CursorOverlayCommand>,
) -> io::Result<()> {
    loop {
        if state.closed {
            return Ok(());
        }

        // Drain pending control commands.
        loop {
            match commands.try_recv() {
                Ok(CursorOverlayCommand::SetEnabled { enabled, reply }) => {
                    state.enabled = enabled;
                    apply_input_region(state, &event_queue.handle());
                    let _ = reply.send(Ok(()));
                }
                Ok(CursorOverlayCommand::SetCursor {
                    visible,
                    x,
                    y,
                    reply,
                }) => {
                    state.pending_cursor_visible = visible;
                    state.pending_cursor_x = x as i32;
                    state.pending_cursor_y = y as i32;
                    draw_cursor(state);
                    let _ = reply.send(Ok(()));
                }
                Ok(CursorOverlayCommand::Stop) => return Ok(()),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // Dispatch already-buffered Wayland events.
        event_queue
            .dispatch_pending(state)
            .map_err(|e| io::Error::other(format!("dispatch: {e}")))?;

        // Flush outgoing requests.
        event_queue
            .flush()
            .map_err(|e| io::Error::other(format!("flush: {e}")))?;

        // Prepare to read more events; if more dispatching is needed first,
        // prepare_read returns None and we loop.
        let Some(guard) = event_queue.prepare_read() else {
            continue;
        };

        // Poll the connection fd with a timeout so we periodically re-check
        // commands even when no Wayland traffic arrives.
        let fd = guard.connection_fd();
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ret = unsafe { libc::poll(&mut pfd, 1, POLL_TIMEOUT_MS) };
        if ret > 0 && (pfd.revents & libc::POLLIN) != 0 {
            guard
                .read()
                .map_err(|e| io::Error::other(format!("read: {e}")))?;
        } else {
            drop(guard);
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch implementations
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, ()> for OverlayState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" => {
                    let version = version.min(5);
                    state.compositor = Some(registry.bind(name, version, qh, ()));
                }
                "wl_seat" => {
                    let version = version.min(9);
                    state.seat = Some(registry.bind(name, version, qh, ()));
                }
                "wl_shm" => {
                    let version = version.min(2);
                    state.shm = Some(registry.bind(name, version, qh, ()));
                }
                "zwlr_layer_shell_v1" => {
                    let version = version.min(4);
                    state.layer_shell = Some(registry.bind(name, version, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_compositor::WlCompositor,
        _event: wl_compositor::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for OverlayState {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let has_pointer = matches!(
                capabilities,
                wayland_client::WEnum::Value(caps) if caps.contains(wl_seat::Capability::Pointer)
            );
            if has_pointer {
                if state.pointer.is_none() {
                    state.pointer = Some(seat.get_pointer(qh, ()));
                }
            } else if let Some(pointer) = state.pointer.take() {
                pointer.release();
            }
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm::WlShm,
        _event: wl_shm::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_shm_pool::WlShmPool,
        _event: wl_shm_pool::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_buffer::WlBuffer,
        _event: wl_buffer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_surface::WlSurface,
        _event: wl_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_region::WlRegion, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for OverlayState {
    fn event(
        state: &mut Self,
        _proxy: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            // Pointer events are only possible on compositors that actually
            // deliver host pointers to nested clients; Gamescope's nested
            // backend does not, and the session-drawn virtual cursor is the
            // primary path. These remain as a secondary source.
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                state.last_x = surface_x;
                state.last_y = surface_y;
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.last_x = surface_x;
                state.last_y = surface_y;
                if let Some((w, h)) = state.configured_size {
                    (state.emit)(CursorEvent::Motion {
                        x: surface_x,
                        y: surface_y,
                        surface_width: w,
                        surface_height: h,
                    });
                }
            }
            wl_pointer::Event::Button {
                button,
                state: button_state,
                ..
            } => {
                if button == BTN_LEFT {
                    let pressed = button_state
                        == wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed);
                    state.left_pressed = pressed;
                    if let Some((w, h)) = state.configured_size {
                        (state.emit)(CursorEvent::LeftButton {
                            pressed,
                            x: state.last_x,
                            y: state.last_y,
                            surface_width: w,
                            surface_height: h,
                        });
                    }
                }
            }
            wl_pointer::Event::Leave { .. } => {
                // Release any held contact so we never leave a stuck touch.
                if state.left_pressed {
                    state.left_pressed = false;
                    if let Some((w, h)) = state.configured_size {
                        (state.emit)(CursorEvent::LeftButton {
                            pressed: false,
                            x: state.last_x,
                            y: state.last_y,
                            surface_width: w,
                            surface_height: h,
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_shell_v1::ZwlrLayerShellV1, ()> for OverlayState {
    fn event(
        _state: &mut Self,
        _proxy: &zwlr_layer_shell_v1::ZwlrLayerShellV1,
        _event: zwlr_layer_shell_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for OverlayState {
    fn event(
        state: &mut Self,
        _proxy: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                if let Some(layer_surface) = state.layer_surface.as_ref() {
                    layer_surface.ack_configure(serial);
                }
                state.configured_size = Some((width, height));
            }
            zwlr_layer_surface_v1::Event::Closed => {
                state.closed = true;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrow_blits_and_clears_within_bounds() {
        let width = 64;
        let height = 64;
        let stride = width * 4;
        let mut bytes = vec![0_u8; (stride * height) as usize];
        blit_arrow(&mut bytes, stride, 10, 20);
        let pixel = |bytes: &[u8], x: i32, y: i32| {
            let start = (y * stride + x * 4) as usize;
            u32::from_ne_bytes(bytes[start..start + 4].try_into().unwrap())
        };
        assert_eq!(pixel(&bytes, 10, 20), CURSOR_OUTLINE);
        assert_eq!(pixel(&bytes, 11, 22), CURSOR_FILL);
        assert_eq!(pixel(&bytes, 30, 30), CURSOR_TRANSPARENT);

        clear_sprite_rect(&mut bytes, stride, Some((10, 20)));
        assert_eq!(pixel(&bytes, 10, 20), CURSOR_TRANSPARENT);
        assert_eq!(pixel(&bytes, 11, 22), CURSOR_TRANSPARENT);
    }
}
