use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::rc::Rc;

use cocoa::appkit::{
    NSApp, NSApplication, NSApplicationActivationPolicyRegular, NSBackingStoreBuffered,
    NSPasteboard, NSView, NSWindow, NSWindowStyleMask,
};
use cocoa::base::{id, nil, BOOL, NO, YES};
use cocoa::foundation::{NSAutoreleasePool, NSPoint, NSRect, NSSize, NSString};
use core_foundation::runloop::{
    CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext, __CFRunLoopTimer, kCFRunLoopDefaultMode,
};
extern "C" {
    fn CFRunLoopGetMain() -> *mut std::ffi::c_void;
    fn CFRunLoopStop(rl: *mut std::ffi::c_void);
    fn CFRunLoopWakeUp(rl: *mut std::ffi::c_void);
}

// `dispatch_get_main_queue()` is a macro that expands to `&_dispatch_main_q`
// in modern SDKs — there is no exported function symbol for it. Reference the
// underlying static directly instead.
#[link(name = "System", kind = "dylib")]
extern "C" {
    static _dispatch_main_q: std::ffi::c_void;
    fn dispatch_async_f(
        queue: *mut std::ffi::c_void,
        context: *mut std::ffi::c_void,
        work: extern "C" fn(*mut std::ffi::c_void),
    );
}

unsafe fn dispatch_main_queue() -> *mut std::ffi::c_void {
    &_dispatch_main_q as *const _ as *mut std::ffi::c_void
}
use keyboard_types::KeyboardEvent;
use objc::class;
use objc::runtime::{Class, Object as ObjcObject};
use objc::{msg_send, runtime::Object, sel, sel_impl};
extern "C" {
    fn class_getInstanceVariable(
        cls: *const Class,
        name: *const std::os::raw::c_char,
    ) -> *const std::os::raw::c_void;
    fn object_getClass(obj: *const ObjcObject) -> *const Class;
}
use raw_window_handle::{
    AppKitDisplayHandle, AppKitWindowHandle, HasRawDisplayHandle, HasRawWindowHandle,
    RawDisplayHandle, RawWindowHandle,
};

use crate::{
    Event, EventStatus, MouseCursor, Size, WindowHandler, WindowInfo, WindowOpenOptions,
    WindowScalePolicy,
};

use super::keyboard::KeyboardState;
use super::view::{create_view, BASEVIEW_STATE_IVAR};

#[cfg(feature = "opengl")]
use crate::gl::{GlConfig, GlContext};

pub struct WindowHandle {
    state: Rc<WindowState>,
}

impl WindowHandle {
    pub fn close(&mut self) {
        self.state.window_inner.close();
    }

    pub fn is_open(&self) -> bool {
        self.state.window_inner.open.get()
    }
}

unsafe impl HasRawWindowHandle for WindowHandle {
    fn raw_window_handle(&self) -> RawWindowHandle {
        self.state.window_inner.raw_window_handle()
    }
}

pub(super) struct WindowInner {
    open: Cell<bool>,

    /// Only set if we created the parent window, i.e. we are running in
    /// parentless mode
    ns_app: Cell<Option<id>>,
    /// Only set if we created the parent window, i.e. we are running in
    /// parentless mode
    ns_window: Cell<Option<id>>,
    /// Our subclassed NSView
    ns_view: id,

    #[cfg(feature = "opengl")]
    gl_context: Option<GlContext>,
}

impl WindowInner {
    pub(super) fn close(&self) {
        if self.open.get() {
            // Run shared teardown (which stops the run loop) BEFORE tearing
            // down the NSWindow. NSWindow's `close` releases its contentView
            // (our NSView), and `close_inner` accesses `self.ns_view` to
            // remove it from the superview, fetch its ivar, etc. — doing
            // that on a released view is UB and silently hangs the close
            // path. Order: cleanup → stop loop → close NSWindow.
            self.close_inner();
            unsafe {
                if let Some(ns_window) = self.ns_window.take() {
                    ns_window.close();
                }
            }
        }
    }

    /// Shared teardown used by both the programmatic `close()` path and the
    /// `windowShouldClose:` delegate path.
    ///
    /// Split into two phases:
    /// - Phase 1 (synchronous, here): mark closed, cancel timer, deregister
    ///   observer, walk up to a baseview ancestor in standalone mode, stop
    ///   the run loop. NON-destructive — leaves the NSView and the
    ///   `Rc<WindowState>` alive so that any in-flight handler (e.g. an
    ///   `on_frame` that's currently calling `window.close()`) can finish
    ///   safely instead of dereferencing freed renderer state.
    /// - Phase 2 (deferred via `dispatch_async`): drop the `Rc<WindowState>`,
    ///   detach + release the NSView. Runs at the start of the next main-queue
    ///   iteration, by which point the in-flight handler has returned.
    pub(super) fn close_inner(&self) {
        if !self.open.get() {
            return;
        }
        self.open.set(false);

        // Snapshot the baseview ancestor (if any) BEFORE we cancel anything,
        // because the superview chain stays intact only until Phase 2 runs.
        let baseview_ancestor = unsafe { find_baseview_ancestor(self.ns_view) };

        unsafe {
            // Cancel the frame timer so no further `on_frame` fires.
            // (Borrow WindowState briefly via the ivar; do NOT drop it here —
            // Phase 2 reclaims it.)
            let state_ptr: *const c_void = *(*self.ns_view).get_ivar(BASEVIEW_STATE_IVAR);
            let window_state_rc = Rc::from_raw(state_ptr as *mut WindowState);
            if let Some(frame_timer) = window_state_rc.frame_timer.take() {
                CFRunLoop::get_current().remove_timer(&frame_timer, kCFRunLoopDefaultMode);
            }
            let _ = Rc::into_raw(window_state_rc);

            let notification_center: id =
                msg_send![class!(NSNotificationCenter), defaultCenter];
            let () = msg_send![notification_center, removeObserver:self.ns_view];

            schedule_finalize_close(self.ns_view);

            // Run-loop teardown / parent propagation.
            let app = self.ns_app.take();
            if let Some(app) = app {
                app.stop_(app);
                let main_loop = CFRunLoopGetMain();
                CFRunLoopStop(main_loop);
                CFRunLoopWakeUp(main_loop);
            } else if let Some(parent_state) = baseview_ancestor {
                // Standalone case: editor parented inside our own outer
                // baseview window — propagate so the wrapper exits.
                parent_state.window_inner.close_inner();
            }
            // Otherwise (DAW host context): the host owns the run loop, leave it alone.
        }
    }

    fn raw_window_handle(&self) -> RawWindowHandle {
        if self.open.get() {
            let ns_window = self.ns_window.get().unwrap_or(ptr::null_mut()) as *mut c_void;

            let mut handle = AppKitWindowHandle::empty();
            handle.ns_window = ns_window;
            handle.ns_view = self.ns_view as *mut c_void;

            return RawWindowHandle::AppKit(handle);
        }

        RawWindowHandle::AppKit(AppKitWindowHandle::empty())
    }
}

/// Phase-2 teardown: drop the `Rc<WindowState>` stored in the NSView's ivar
/// and release the NSView itself. Runs on the main queue *after* the current
/// run-loop iteration, so any in-flight `on_frame` (whose `window.close()`
/// triggered the close) has fully returned before its `WindowState` and
/// renderer are dropped.
extern "C" fn finalize_close_callback(ns_view_ctx: *mut std::ffi::c_void) {
    unsafe {
        let ns_view = ns_view_ctx as id;
        // Reclaim the Rc<WindowState> from the ivar and drop it. This frees
        // the WindowHandler (egui renderer, GL context, etc.).
        let state_ptr: *const c_void = *(*ns_view).get_ivar(BASEVIEW_STATE_IVAR);
        if !state_ptr.is_null() {
            let window_state = Rc::from_raw(state_ptr as *const WindowState);
            // Null out the ivar so we never reclaim twice.
            (*ns_view).set_ivar(BASEVIEW_STATE_IVAR, ptr::null::<c_void>() as *const c_void);
            drop(window_state);
        }
        // Detach from the parent and release our retain.
        let _: () = msg_send![ns_view, removeFromSuperview];
        let () = msg_send![ns_view, release];
    }
}

unsafe fn schedule_finalize_close(ns_view: id) {
    dispatch_async_f(
        dispatch_main_queue(),
        ns_view as *mut std::ffi::c_void,
        finalize_close_callback,
    );
}

/// Walk up the superview chain looking for an NSView whose class registered
/// `BASEVIEW_STATE_IVAR` — i.e. another baseview-owned view. If found,
/// returns a borrowed reference to that view's `WindowInner`.
///
/// Used by parented `close_inner` so that, in standalone mode (where the
/// editor is parented inside our own outer baseview window), closing the
/// editor also closes the outer window. In a DAW host the parent NSView
/// belongs to the host and won't have the ivar, so we leave it alone.
unsafe fn find_baseview_ancestor(view: id) -> Option<Rc<WindowState>> {
    let ivar_name = std::ffi::CString::new(BASEVIEW_STATE_IVAR).ok()?;
    let mut current: id = msg_send![view, superview];
    while current != nil {
        let class_ptr = object_getClass(current as *const ObjcObject);
        if !class_ptr.is_null() {
            let ivar_ptr = class_getInstanceVariable(class_ptr, ivar_name.as_ptr());
            if !ivar_ptr.is_null() {
                // This NSView's class registered our ivar — it's a baseview view.
                let state_ptr: *const c_void = *(*current).get_ivar(BASEVIEW_STATE_IVAR);
                if !state_ptr.is_null() {
                    let state_rc = Rc::from_raw(state_ptr as *const WindowState);
                    let cloned = Rc::clone(&state_rc);
                    let _ = Rc::into_raw(state_rc);
                    return Some(cloned);
                }
            }
        }
        current = msg_send![current, superview];
    }
    None
}

pub struct Window<'a> {
    inner: &'a WindowInner,
}

impl<'a> Window<'a> {
    pub fn open_parented<P, H, B>(parent: &P, options: WindowOpenOptions, build: B) -> WindowHandle
    where
        P: HasRawWindowHandle,
        H: WindowHandler + 'static,
        B: FnOnce(&mut crate::Window) -> H,
        B: Send + 'static,
    {
        let pool = unsafe { NSAutoreleasePool::new(nil) };

        let scaling = match options.scale {
            WindowScalePolicy::ScaleFactor(scale) => scale,
            WindowScalePolicy::SystemScaleFactor => 1.0,
        };

        let window_info = WindowInfo::from_logical_size(options.size, scaling);

        let handle = if let RawWindowHandle::AppKit(handle) = parent.raw_window_handle() {
            handle
        } else {
            panic!("Not a macOS window");
        };

        let ns_view = unsafe { create_view(&options) };

        let window_inner = WindowInner {
            open: Cell::new(true),
            ns_app: Cell::new(None),
            ns_window: Cell::new(None),
            ns_view,

            #[cfg(feature = "opengl")]
            gl_context: options
                .gl_config
                .map(|gl_config| Self::create_gl_context(None, ns_view, gl_config)),
        };

        let window_handle = Self::init(window_inner, window_info, build);

        unsafe {
            let _: id = msg_send![handle.ns_view as *mut Object, addSubview: ns_view];

            let () = msg_send![pool, drain];
        }

        window_handle
    }

    pub fn open_blocking<H, B>(options: WindowOpenOptions, build: B)
    where
        H: WindowHandler + 'static,
        B: FnOnce(&mut crate::Window) -> H,
        B: Send + 'static,
    {
        let pool = unsafe { NSAutoreleasePool::new(nil) };

        // It seems prudent to run NSApp() here before doing other
        // work. It runs [NSApplication sharedApplication], which is
        // what is run at the very start of the Xcode-generated main
        // function of a cocoa app according to:
        // https://developer.apple.com/documentation/appkit/nsapplication
        let app = unsafe { NSApp() };

        unsafe {
            app.setActivationPolicy_(NSApplicationActivationPolicyRegular);
            // Bring the app to the foreground. Without this, on macOS a
            // standalone app launched from a terminal sometimes appears
            // behind other apps (visible only via Exposé) and clicking the
            // Dock icon does not bring it forward. `setActivationPolicy:`
            // alone doesn't activate; an explicit `activateIgnoringOtherApps:`
            // is required.
            let () = msg_send![app, activateIgnoringOtherApps: YES];
        }

        let scaling = match options.scale {
            WindowScalePolicy::ScaleFactor(scale) => scale,
            WindowScalePolicy::SystemScaleFactor => 1.0,
        };

        let window_info = WindowInfo::from_logical_size(options.size, scaling);

        let rect = NSRect::new(
            NSPoint::new(0.0, 0.0),
            NSSize::new(window_info.logical_size().width, window_info.logical_size().height),
        );

        let ns_window = unsafe {
            let ns_window = NSWindow::alloc(nil).initWithContentRect_styleMask_backing_defer_(
                rect,
                NSWindowStyleMask::NSTitledWindowMask
                    | NSWindowStyleMask::NSClosableWindowMask
                    | NSWindowStyleMask::NSMiniaturizableWindowMask,
                NSBackingStoreBuffered,
                NO,
            );
            ns_window.center();

            let title = NSString::alloc(nil).init_str(&options.title).autorelease();
            ns_window.setTitle_(title);

            ns_window.makeKeyAndOrderFront_(nil);

            ns_window
        };

        let ns_view = unsafe { create_view(&options) };

        let window_inner = WindowInner {
            open: Cell::new(true),
            ns_app: Cell::new(Some(app)),
            ns_window: Cell::new(Some(ns_window)),
            ns_view,

            #[cfg(feature = "opengl")]
            gl_context: options
                .gl_config
                .map(|gl_config| Self::create_gl_context(Some(ns_window), ns_view, gl_config)),
        };

        let _ = Self::init(window_inner, window_info, build);

        unsafe {
            ns_window.setContentView_(ns_view);
            ns_window.setDelegate_(ns_view);

            let () = msg_send![pool, drain];

            app.run();
        }
    }

    fn init<H, B>(window_inner: WindowInner, window_info: WindowInfo, build: B) -> WindowHandle
    where
        H: WindowHandler + 'static,
        B: FnOnce(&mut crate::Window) -> H,
        B: Send + 'static,
    {
        let mut window = crate::Window::new(Window { inner: &window_inner });
        let window_handler = Box::new(build(&mut window));

        let ns_view = window_inner.ns_view;

        let window_state = Rc::new(WindowState {
            window_inner,
            window_handler: RefCell::new(window_handler),
            keyboard_state: KeyboardState::new(),
            frame_timer: Cell::new(None),
            window_info: Cell::new(window_info),
            deferred_events: RefCell::default(),
        });

        let window_state_ptr = Rc::into_raw(Rc::clone(&window_state));

        unsafe {
            (*ns_view).set_ivar(BASEVIEW_STATE_IVAR, window_state_ptr as *const c_void);

            WindowState::setup_timer(window_state_ptr);
        }

        WindowHandle { state: window_state }
    }

    pub fn close(&mut self) {
        self.inner.close();
    }

    pub fn has_focus(&mut self) -> bool {
        unsafe {
            let view = self.inner.ns_view.as_mut().unwrap();
            let window: id = msg_send![view, window];
            if window == nil {
                return false;
            };
            let first_responder: id = msg_send![window, firstResponder];
            let is_key_window: BOOL = msg_send![window, isKeyWindow];
            let is_focused: BOOL = msg_send![view, isEqual: first_responder];
            is_key_window == YES && is_focused == YES
        }
    }

    pub fn focus(&mut self) {
        unsafe {
            let view = self.inner.ns_view.as_mut().unwrap();
            let window: id = msg_send![view, window];
            if window != nil {
                msg_send![window, makeFirstResponder:view]
            }
        }
    }

    pub fn resize(&mut self, size: Size) {
        if self.inner.open.get() {
            // NOTE: macOS gives you a personal rave if you pass in fractional pixels here. Even
            // though the size is in fractional pixels.
            let size = NSSize::new(size.width.round(), size.height.round());

            unsafe { NSView::setFrameSize(self.inner.ns_view, size) };
            unsafe {
                let _: () = msg_send![self.inner.ns_view, setNeedsDisplay: YES];
            }

            // When using OpenGL the `NSOpenGLView` needs to be resized separately? Why? Because
            // macOS.
            #[cfg(feature = "opengl")]
            if let Some(gl_context) = &self.inner.gl_context {
                gl_context.resize(size);
            }

            // If this is a standalone window then we'll also need to resize the window itself
            if let Some(ns_window) = self.inner.ns_window.get() {
                unsafe { NSWindow::setContentSize_(ns_window, size) };
            }
        }
    }

    pub fn set_mouse_cursor(&mut self, _mouse_cursor: MouseCursor) {
        todo!()
    }

    #[cfg(feature = "opengl")]
    pub fn gl_context(&self) -> Option<&GlContext> {
        self.inner.gl_context.as_ref()
    }

    #[cfg(feature = "opengl")]
    fn create_gl_context(ns_window: Option<id>, ns_view: id, config: GlConfig) -> GlContext {
        let mut handle = AppKitWindowHandle::empty();
        handle.ns_window = ns_window.unwrap_or(ptr::null_mut()) as *mut c_void;
        handle.ns_view = ns_view as *mut c_void;
        let handle = RawWindowHandle::AppKit(handle);

        unsafe { GlContext::create(&handle, config).expect("Could not create OpenGL context") }
    }
}

pub(super) struct WindowState {
    pub(super) window_inner: WindowInner,
    window_handler: RefCell<Box<dyn WindowHandler>>,
    keyboard_state: KeyboardState,
    frame_timer: Cell<Option<CFRunLoopTimer>>,
    /// The last known window info for this window.
    pub window_info: Cell<WindowInfo>,

    /// Events that will be triggered at the end of `window_handler`'s borrow.
    deferred_events: RefCell<VecDeque<Event>>,
}

impl WindowState {
    /// Gets the `WindowState` held by a given `NSView`.
    ///
    /// This method returns a cloned `Rc<WindowState>` rather than just a `&WindowState`, since the
    /// original `Rc<WindowState>` owned by the `NSView` can be dropped at any time
    /// (including during an event handler).
    pub(super) unsafe fn from_view(view: &Object) -> Rc<WindowState> {
        let state_ptr: *const c_void = *view.get_ivar(BASEVIEW_STATE_IVAR);

        let state_rc = Rc::from_raw(state_ptr as *const WindowState);
        let state = Rc::clone(&state_rc);
        let _ = Rc::into_raw(state_rc);

        state
    }

    /// Trigger the event immediately and return the event status.
    /// Will panic if `window_handler` is already borrowed (see `trigger_deferrable_event`).
    pub(super) fn trigger_event(&self, event: Event) -> EventStatus {
        let mut window = crate::Window::new(Window { inner: &self.window_inner });
        let mut window_handler = self.window_handler.borrow_mut();
        let status = window_handler.on_event(&mut window, event);
        self.send_deferred_events(window_handler.as_mut());
        status
    }

    /// Trigger the event immediately if `window_handler` can be borrowed mutably,
    /// otherwise add the event to a queue that will be cleared once `window_handler`'s mutable borrow ends.
    /// As this method might result in the event triggering asynchronously, it can't reliably return the event status.
    pub(super) fn trigger_deferrable_event(&self, event: Event) {
        if let Ok(mut window_handler) = self.window_handler.try_borrow_mut() {
            let mut window = crate::Window::new(Window { inner: &self.window_inner });
            window_handler.on_event(&mut window, event);
            self.send_deferred_events(window_handler.as_mut());
        } else {
            self.deferred_events.borrow_mut().push_back(event);
        }
    }

    pub(super) fn trigger_frame(&self) {
        let mut window = crate::Window::new(Window { inner: &self.window_inner });
        let mut window_handler = self.window_handler.borrow_mut();
        window_handler.on_frame(&mut window);
        self.send_deferred_events(window_handler.as_mut());
    }

    pub(super) fn keyboard_state(&self) -> &KeyboardState {
        &self.keyboard_state
    }

    pub(super) fn process_native_key_event(&self, event: *mut Object) -> Option<KeyboardEvent> {
        self.keyboard_state.process_native_event(event)
    }

    unsafe fn setup_timer(window_state_ptr: *const WindowState) {
        extern "C" fn timer_callback(_: *mut __CFRunLoopTimer, window_state_ptr: *mut c_void) {
            unsafe {
                let window_state = &*(window_state_ptr as *const WindowState);

                window_state.trigger_frame();
            }
        }

        let mut timer_context = CFRunLoopTimerContext {
            version: 0,
            info: window_state_ptr as *mut c_void,
            retain: None,
            release: None,
            copyDescription: None,
        };

        let timer = CFRunLoopTimer::new(0.0, 0.015, 0, 0, timer_callback, &mut timer_context);

        CFRunLoop::get_current().add_timer(&timer, kCFRunLoopDefaultMode);

        (*window_state_ptr).frame_timer.set(Some(timer));
    }

    fn send_deferred_events(&self, window_handler: &mut dyn WindowHandler) {
        let mut window = crate::Window::new(Window { inner: &self.window_inner });
        loop {
            let next_event = self.deferred_events.borrow_mut().pop_front();
            if let Some(event) = next_event {
                window_handler.on_event(&mut window, event);
            } else {
                break;
            }
        }
    }
}

unsafe impl<'a> HasRawWindowHandle for Window<'a> {
    fn raw_window_handle(&self) -> RawWindowHandle {
        self.inner.raw_window_handle()
    }
}

unsafe impl<'a> HasRawDisplayHandle for Window<'a> {
    fn raw_display_handle(&self) -> RawDisplayHandle {
        RawDisplayHandle::AppKit(AppKitDisplayHandle::empty())
    }
}

pub fn copy_to_clipboard(string: &str) {
    unsafe {
        let pb = NSPasteboard::generalPasteboard(nil);

        let ns_str = NSString::alloc(nil).init_str(string);

        pb.clearContents();
        pb.setString_forType(ns_str, cocoa::appkit::NSPasteboardTypeString);
    }
}
