# Custom Title Bar Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers-optimized:subagent-driven-development (recommended) or superpowers-optimized:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the native Windows caption with a compact, accessible Slint title bar that preserves native window movement and lifecycle behavior.
**Architecture:** Slint owns presentation and emits four window callbacks. Rust delegates drag to the selected Winit backend and minimize/maximize/close to the Slint window/event loop.
**Tech Stack:** Rust 2024, Slint 1.17, Winit 0.30 through Slint.
**Assumptions:** The production backend is Winit on Windows x64 — drag will not work on a non-Winit backend, while other controls remain functional.

## Files

- Modify `apps/multicore-desktop/ui/app.slint` for the frameless title bar.
- Modify `apps/multicore-desktop/src/main.rs` for native window callbacks.
- Modify `apps/multicore-desktop/Cargo.toml` to expose Slint's pinned Winit accessor.
- Test in `apps/multicore-desktop/src/main.rs` and existing deterministic preview tooling.

### Task 1: Lock window-state behavior

**Security flag:** none

**Does NOT cover:** Native dragging; it requires an active event-loop window and is verified by compilation/smoke rather than a headless unit test.

- [ ] Add a failing unit test for `next_maximized(false) == true` and `next_maximized(true) == false`.
- [ ] Run `cargo test -p multicore-desktop next_maximized` and observe the missing helper failure.
- [ ] Add `fn next_maximized(current: bool) -> bool { !current }` and re-run the target test.

### Task 2: Add the frameless shell

**Security flag:** none

- [ ] Enable Slint's `unstable-winit-030` feature without changing its pinned version.
- [ ] Set `no-frame: true`, add callbacks `window-drag`, `window-minimize`, `window-toggle-maximize`, and `window-close`, and wrap existing content below a 44 px title bar.
- [ ] Wire callbacks using `WinitWindowAccessor::with_winit_window`, `Window::set_minimized`, `Window::set_maximized`, and `slint::quit_event_loop`.
- [ ] Run `cargo test -p multicore-desktop` and `cargo build -p multicore-desktop --release --locked`.

### Task 3: Visual and lifecycle verification

**Security flag:** none

- [ ] Capture ready and minimum-width preview fixtures and inspect title-bar alignment, focus borders, hit targets, and content height.
- [ ] Run the packaged no-profile one-click smoke and confirm graceful desktop/daemon cleanup without starting cores or TUN.
