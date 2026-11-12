// Browser dialog bridge for the GML show_message / show_message_ext / show_question / get_string /
// get_integer family.
//
// Those GML functions are synchronous: they block the game and (for question/input) return a value
// inline, mid-frame. The engine runs in a worker, so `gems::show_dialog` blocks this thread on the
// host's shared dialog buffer until the player answers.

// Input-field kinds, as `gems::show_dialog` takes them.
pub const INPUT_NONE: i32 = 0;
pub const INPUT_STRING: i32 = 1;
pub const INPUT_INTEGER: i32 = 2;

/// Shows a dialog and blocks until answered. Returns the 0-based index of the clicked
/// button and the input field's text (empty when there's no field). `labels` are the button
/// captions; `primary`/`cancel` index into them (or -1) for the Enter/Escape defaults.
pub fn show(
    text: &str,
    labels: &[&str],
    primary: i32,
    cancel: i32,
    input_kind: i32,
    input_initial: &str,
) -> (i32, String) {
    // GM8 treats '#' as a line break in message text (same convention as draw_text, see
    // game::draw::LineIterator), and '\#' as an escaped literal '#'.
    let text = hash_newlines(text);
    // Joined with U+001F (unit separator), which won't occur in game text.
    let joined = labels.join("\u{1f}");
    crate::gems::show_dialog(&text, &joined, primary, cancel, input_kind, input_initial)
}

fn hash_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '#' => out.push('\n'),
            '\\' if chars.peek() == Some(&'#') => {
                chars.next();
                out.push('#');
            },
            _ => out.push(c),
        }
    }
    out
}
