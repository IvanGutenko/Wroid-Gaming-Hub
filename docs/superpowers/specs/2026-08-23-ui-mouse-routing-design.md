# UI Mouse Routing Design

Date: 2026-08-23
Status: approved by direct implementation request

## Problems

The game-session loop derives `InputMode` from both the profile aim state and
the current `CursorOverlay` health. If the overlay fails after UI mode starts,
the next keyboard event selects `Aim` even though the profile's toggle aim is
inactive. Physical LMB is then dispatched to the profile `fire` hold instead
of the Android UI cursor contact.

The cursor overlay also allocates full-surface buffers in one SHM pool,
but writes both frames through the pool's zero offset. Commits of the second
buffer therefore display transparent memory. The renderer also mutates a
buffer without waiting for `wl_buffer.release`. A later two-buffer lifecycle
fix still stalls at the initial position because Gamescope can retain the
startup frame and the currently presented frame simultaneously.

## Design

Overlay availability is a startup capability decision, not live routing
state. Capture it once as `ui_mode_supported` when the event loop starts.
Thereafter, `InputMode` depends only on that fixed capability and the runtime
aim state. `OverlayGuard` remains a best-effort renderer: losing it hides the
sprite and emits one warning, but cannot turn UI input into gameplay input.

The cursor renderer addresses each buffer's own SHM frame. A three-slot state
machine marks buffers busy at commit, releases them only on
`wl_buffer.release`, and retains the newest requested cursor position while
all buffers are busy. The third frame remains writable while Gamescope retains
the previous and current frames. Aim mode continues to send mouse events to profile
bindings. UI mode continues to consume relative motion and LMB itself and to
inject the dedicated cursor contact. No profile format, contact allocation,
or Android injection changes are required.

## Testing

An event-loop regression test starts in supported UI mode with the overlay
already unavailable, sends an unrelated keyboard event, then LMB down/up. It
must inject at the virtual cursor position and must never touch the profile
fire coordinate. Existing aim-mode mouse binding tests remain unchanged.

Renderer unit tests verify that painting buffer 2 changes only the third SHM
frame, that the initial and presented frames leave a third writable slot, and
that no slot is reused before its release event.
