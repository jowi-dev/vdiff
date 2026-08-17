//! Pure display-label formatting for a node id. Shared between
//! [`crate::graph::layout`] (label-fit box sizing) and
//! [`crate::ui::graph_view`] (painting) -- it's a plain string function with
//! no egui dependency, so it belongs here rather than in `src/ui`.

/// Abbreviate `id`'s qualified name for display by stripping its top-level
/// root's own qualified prefix: `elixir:App.Leads.Lead` under root
/// `elixir:App` becomes `Leads.Lead`. `id` naming `root_id` itself (the
/// root's own module) shows `display_name` plain, matching every other
/// node's own name. Falls back to `id`'s body (language prefix stripped)
/// if it doesn't actually start with the root's prefix (shouldn't happen
/// for a well-formed graph, but avoids stripping the wrong thing).
pub fn abbreviated_label(id: &str, root_id: &str, display_name: &str) -> String {
    let id_body = strip_lang_prefix(id);
    let root_body = strip_lang_prefix(root_id);

    if id_body == root_body {
        return display_name.to_string();
    }

    let sep = lang_separator(id);
    let prefix = format!("{root_body}{sep}");
    match id_body.strip_prefix(&prefix) {
        Some(rest) => rest.to_string(),
        None => id_body.to_string(),
    }
}

/// Strip a `lang:` namespace prefix (`elixir:`, `rust:`, `file:` -- see
/// [`crate::graph::builder`]) off an id string, if present.
fn strip_lang_prefix(id: &str) -> &str {
    match id.find(':') {
        Some(idx) => &id[idx + 1..],
        None => id,
    }
}

/// The path separator a given id's language namespace uses between
/// segments, so [`abbreviated_label`] strips exactly the root prefix and
/// not part of the next segment's name.
fn lang_separator(id: &str) -> &'static str {
    if id.starts_with("rust:") {
        "::"
    } else if id.starts_with("file:") {
        "/"
    } else {
        // elixir: and anything unrecognized.
        "."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviated_label_strips_elixir_root_prefix() {
        assert_eq!(
            abbreviated_label("elixir:App.Leads.Lead", "elixir:App", "Lead"),
            "Leads.Lead"
        );
    }

    #[test]
    fn abbreviated_label_shows_plain_name_for_the_root_itself() {
        assert_eq!(abbreviated_label("elixir:App", "elixir:App", "App"), "App");
    }

    #[test]
    fn abbreviated_label_strips_rust_root_prefix() {
        assert_eq!(
            abbreviated_label("rust:crate_a::foo::bar", "rust:crate_a", "bar"),
            "foo::bar"
        );
    }

    #[test]
    fn abbreviated_label_falls_back_to_full_body_when_not_prefixed() {
        assert_eq!(
            abbreviated_label("elixir:Other.Thing", "elixir:App", "Thing"),
            "Other.Thing"
        );
    }
}
