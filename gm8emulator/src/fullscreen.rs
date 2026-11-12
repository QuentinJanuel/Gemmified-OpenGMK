// Browser fullscreen bridge for the GML window_set_fullscreen/window_get_fullscreen builtins, over
// the host's window services (`gems`). `get` answers from the host's last reported state, so it is
// current mid-frame.

pub fn set(enabled: bool) {
    crate::gems::set_fullscreen(enabled);
}

pub fn get() -> bool {
    crate::gems::is_fullscreen()
}
