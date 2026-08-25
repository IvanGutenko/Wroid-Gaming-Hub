# UI Mouse Routing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep physical LMB routed to the Android UI whenever the session is logically in UI mode, even if the Wayland cursor overlay fails.

**Architecture:** Freeze UI-mode support at event-loop startup and derive later mode transitions only from that capability and profile aim activity. Treat `OverlayGuard` solely as a fallible sprite renderer.

**Tech Stack:** Rust 2021, existing `wroid-inject` event loop and recording injector.

## Global Constraints

- Aim mode retains existing profile mouse bindings.
- UI mode maps LMB to the dedicated cursor contact at the virtual cursor position.
- Overlay failure must not change input routing.
- Each `wl_buffer` must be painted at its own SHM offset.
- A busy `wl_buffer` must not be mutated before `wl_buffer.release`.
- No profile schema or Android bridge protocol changes.

---

### Task 1: Decouple UI routing from overlay health

**Files:**
- Modify: `crates/wroid-inject/src/game_session.rs`
- Test: `crates/wroid-inject/src/game_session.rs`

**Interfaces:**
- Produces: fixed event-loop boolean `ui_mode_supported` derived from `initial_mode`.
- Removes: live routing dependency on `OverlayGuard::is_available()`.

- [x] **Step 1: Write the failing regression test**

Create a toggle-aim profile with an LMB hold at `(900, 470)`. Start the loop in
UI mode with no overlay, send an unrelated Shift key event, LMB down/up, and
the exit hotkey. Assert the touch frames are Down/Up at the centered virtual
cursor `(500, 500)`, never at the fire point.

- [x] **Step 2: Verify RED**

Run `cargo test -p wroid-inject --lib ui_lmb_stays_cursor_click_after_overlay_loss -- --nocapture`.
Expected: the current loop changes to Aim and emits the fire coordinate.

- [x] **Step 3: Implement the minimal fix**

Compute `let ui_mode_supported = options.initial_mode == InputMode::Ui;` once.
Make `desired_mode` use this fixed value instead of
`overlay.is_available()`. Remove the now-unused availability method.

- [x] **Step 4: Verify GREEN and regressions**

Run the focused test, the `wroid-inject` library suite, workspace formatting,
Clippy/check/test, and release builds requested by the user.

---

### Task 2: Repair the cursor overlay double-buffer lifecycle

**Files:**
- Modify: `crates/wroid-inject/src/cursor_overlay.rs`
- Test: `crates/wroid-inject/src/cursor_overlay.rs`

- [x] **Step 1: Add failing SHM-frame and buffer-lifecycle tests**

Verify that painting slot 1 leaves slot 0 untouched, and that two acquired
slots return no third slot until a release is received.

- [x] **Step 2: Paint the selected SHM frame**

Slice the mapped pool using `frame_length * buffer_index` before clearing and
blitting the cursor.

- [x] **Step 3: Honor `wl_buffer.release`**

Track both slots as busy after commit, coalesce motion to the newest pending
position while both are busy, and render again from the release callback.

- [x] **Step 4: Verify and install**

Run focused RED/GREEN tests, the full workspace format/check/Clippy/test/build
suite, then install and byte-verify the release binaries and helper.

---

### Task 3: Keep a writable cursor frame under Gamescope retention

**Files:**
- Modify: `crates/wroid-inject/src/cursor_overlay.rs`
- Test: `crates/wroid-inject/src/cursor_overlay.rs`

- [x] **Step 1: Reproduce the initial-position stall**

Acquire the transparent startup frame and first visible frame, then assert
that the first movement can still acquire a third slot. The two-slot pool must
fail this assertion.

- [x] **Step 2: Use three release-gated SHM frames**

Allocate a third full-surface frame and keep the existing rule that a frame is
not painted again until its `wl_buffer.release` event arrives.

- [x] **Step 3: Verify and install**

Run focused and workspace-wide verification, build release artifacts, then
install and byte-verify the CLI, daemon, and privileged helper.
