use cef::*;
use std::sync::LazyLock;
use x11_dl::xlib;

use crate::cef_webview::CefBrowserExt;

static X11: LazyLock<Option<xlib::Xlib>> = LazyLock::new(|| xlib::Xlib::open().ok());

/// Walk up from `window` to the ancestor that sits directly inside the toplevel,
/// i.e. the last window before a child of the X root. CEF wraps a windowed
/// browser in an extra window, and only that wrapper's geometry is honoured by
/// the parent, so bounds changes have to target it rather than the handle CEF
/// reports. Returns `window` itself when it is already that ancestor (or when
/// the tree cannot be queried).
#[cfg(target_os = "linux")]
unsafe fn outermost_child(
  xlib: &xlib::Xlib,
  display: *mut xlib::Display,
  window: xlib::Window,
) -> xlib::Window {
  let root = (xlib.XDefaultRootWindow)(display);
  let mut current = window;
  // Bounded so a malformed tree cannot spin here.
  for _ in 0..8 {
    let mut tree_root: xlib::Window = 0;
    let mut parent: xlib::Window = 0;
    let mut children: *mut xlib::Window = std::ptr::null_mut();
    let mut nchildren: u32 = 0;
    let status = (xlib.XQueryTree)(
      display,
      current,
      &mut tree_root,
      &mut parent,
      &mut children,
      &mut nchildren,
    );
    if !children.is_null() {
      (xlib.XFree)(children as *mut std::ffi::c_void);
    }
    if status == 0 || parent == 0 || parent == root {
      // `current`'s parent is the root, so `current` is the toplevel itself:
      // the wrapper is the level we came from, which is what the loop returns.
      return current;
    }

    // Is the parent a child of the root? Then `current` is the wrapper.
    let mut p_root: xlib::Window = 0;
    let mut grandparent: xlib::Window = 0;
    let mut p_children: *mut xlib::Window = std::ptr::null_mut();
    let mut p_nchildren: u32 = 0;
    let p_status = (xlib.XQueryTree)(
      display,
      parent,
      &mut p_root,
      &mut grandparent,
      &mut p_children,
      &mut p_nchildren,
    );
    if !p_children.is_null() {
      (xlib.XFree)(p_children as *mut std::ffi::c_void);
    }
    if p_status != 0 && grandparent == root {
      return current;
    }
    current = parent;
  }
  window
}

impl CefBrowserExt for cef::Browser {
  fn xid(&self) -> Option<u64> {
    let host = self.host()?;
    let xid = host.window_handle();
    Some(xid)
  }

  fn bounds(&self) -> cef::Rect {
    let Some(xid) = self.xid() else {
      return cef::Rect::default();
    };

    let Some(xlib) = X11.as_ref() else {
      return cef::Rect::default();
    };

    unsafe {
      let display = (xlib.XOpenDisplay)(std::ptr::null());
      if display.is_null() {
        return cef::Rect::default();
      }

      let mut root: xlib::Window = 0;
      let mut x: i32 = 0;
      let mut y: i32 = 0;
      let mut width: u32 = 0;
      let mut height: u32 = 0;
      let mut border_width: u32 = 0;
      let mut depth: u32 = 0;

      let status = (xlib.XGetGeometry)(
        display,
        xid as xlib::Window,
        &mut root,
        &mut x,
        &mut y,
        &mut width,
        &mut height,
        &mut border_width,
        &mut depth,
      );

      (xlib.XCloseDisplay)(display);

      if status == 0 {
        return cef::Rect::default();
      }

      // XGetGeometry returns position relative to parent, which is what we need
      cef::Rect {
        x,
        y,
        width: width as i32,
        height: height as i32,
      }
    }
  }

  fn set_bounds(&self, rect: Option<&cef::Rect>) {
    let Some(rect) = rect else {
      return;
    };

    let Some(xid) = self.xid() else {
      return;
    };

    let Some(xlib) = X11.as_ref() else {
      return;
    };

    unsafe {
      let display = (xlib.XOpenDisplay)(std::ptr::null());
      if display.is_null() {
        return;
      }

      // CEF nests the browser inside its own wrapper window, so the handle it
      // hands back is not the window whose geometry the parent honours:
      //
      //   app toplevel ── CEF wrapper (this is what must be moved/resized)
      //                    └── browser window (what `window_handle()` returns)
      //
      // Resizing only the inner window left the wrapper at its spawn geometry,
      // which is why bounds updates looked like no-ops and the webview never
      // followed the window. Walk up to the wrapper — the ancestor whose own
      // parent is the toplevel, i.e. a grandchild of the X root — and move
      // that, then match the inner window to it.
      let outer = outermost_child(xlib, display, xid as xlib::Window);

      (xlib.XMoveResizeWindow)(
        display,
        outer,
        rect.x,
        rect.y,
        rect.width as u32,
        rect.height as u32,
      );
      if outer != xid as xlib::Window {
        (xlib.XMoveResizeWindow)(
          display,
          xid as xlib::Window,
          0,
          0,
          rect.width as u32,
          rect.height as u32,
        );
      }
      // Ensure window is mapped and raised after setting bounds
      (xlib.XMapRaised)(display, outer);
      (xlib.XFlush)(display);
      (xlib.XCloseDisplay)(display);
    }
  }

  fn scale_factor(&self) -> f64 {
    // Get scale factor from primary display
    // CEF on Linux doesn't provide direct access to the window's display,
    // so we use the primary display as a reasonable default
    cef::display_get_primary()
      .map(|d| d.device_scale_factor() as f64)
      .unwrap_or(1.0)
  }

  fn set_visible(&self, visible: i32) {
    let Some(xid) = self.xid() else {
      return;
    };

    let Some(xlib) = X11.as_ref() else {
      return;
    };

    unsafe {
      let display = (xlib.XOpenDisplay)(std::ptr::null());
      if display.is_null() {
        return;
      }

      if visible != 0 {
        (xlib.XMapWindow)(display, xid as xlib::Window);
      } else {
        (xlib.XUnmapWindow)(display, xid as xlib::Window);
      }
      (xlib.XFlush)(display);
      (xlib.XCloseDisplay)(display);
    }
  }

  fn close(&self) {
    // Ask CEF to tear the browser down first. Destroying only the X window is
    // not enough — under Wayland `xid` is not a live X window at all, so an
    // XDestroyWindow-only close left the browser (and its renderer process)
    // running forever, and every reopen leaked another copy of the page.
    if let Some(host) = self.host() {
      host.close_dev_tools();
      host.try_close_browser();
    }

    let Some(xid) = self.xid() else {
      return;
    };

    let Some(xlib) = X11.as_ref() else {
      return;
    };

    unsafe {
      let display = (xlib.XOpenDisplay)(std::ptr::null());
      if display.is_null() {
        return;
      }

      (xlib.XDestroyWindow)(display, xid as xlib::Window);
      (xlib.XFlush)(display);
      (xlib.XCloseDisplay)(display);
    }
  }

  fn set_parent(&self, parent: &cef::Window) {
    let Some(xid) = self.xid() else {
      return;
    };

    let parent_xid = parent.window_handle();
    if parent_xid == 0 {
      return;
    }

    let Some(xlib) = X11.as_ref() else {
      return;
    };

    unsafe {
      let display = (xlib.XOpenDisplay)(std::ptr::null());
      if display.is_null() {
        return;
      }

      // Check if window exists before reparenting
      let mut root: xlib::Window = 0;
      let mut parent_window: xlib::Window = 0;
      let mut children: *mut xlib::Window = std::ptr::null_mut();
      let mut nchildren: u32 = 0;
      let status = (xlib.XQueryTree)(
        display,
        xid as xlib::Window,
        &mut root,
        &mut parent_window,
        &mut children,
        &mut nchildren,
      );

      if status != 0 && !children.is_null() {
        (xlib.XFree)(children as *mut std::ffi::c_void);
      }

      (xlib.XReparentWindow)(
        display,
        xid as xlib::Window,
        parent_xid as xlib::Window,
        0,
        0,
      );

      // Ensure window is mapped and raised after reparenting
      (xlib.XMapRaised)(display, xid as xlib::Window);
      (xlib.XFlush)(display);
      (xlib.XCloseDisplay)(display);
    }
  }
}
