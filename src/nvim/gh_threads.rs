//! Inline display of GitHub PR review threads (issue #35) in the embedded
//! nvim session: [`threads_lua`] builds one Lua chunk that hands nvim every
//! placeable thread and installs a `BufEnter` autocmd placing them as
//! virtual lines under their anchored line, in a `vdiff_gh` namespace of
//! their own. Frontend-neutral and pure (string building only); each
//! frontend sends the chunk via [`crate::nvim::session::NvimCmd::ExecLua`].
//!
//! A separate namespace matters: the open-file Lua clears the `vdiff`
//! namespace on every open, and thread marks must survive that. The
//! autocmd makes the marks follow whatever buffer the user lands on --
//! vdiff's own opens, `:e`, `gf`, jumps -- without vdiff tracking which
//! file is showing. Sending the chunk again replaces the data and re-marks
//! every loaded buffer, so a refresh is just another send.
//!
//! Comment bodies are arbitrary text from anyone who can comment on the
//! PR, so every string goes through [`lua_quote`], which escapes every
//! byte outside a small safe set rather than trying to enumerate the
//! dangerous ones.

use crate::review::gh_threads::{inline_line, PrThreads, ReviewThread};

/// Lua that installs the data-driven marking: `apply(buf)` clears the
/// `vdiff_gh` namespace in `buf` and, when the buffer's name is or ends in
/// `/<path>` for a thread path, places each of that path's threads as
/// virtual lines under its line (clamped to the buffer, `pcall`-wrapped so
/// a stale line never raises). Expects a `data` table in scope, keyed by
/// repo-relative path.
const APPLY_LUA: &str = r#"
local ns = vim.api.nvim_create_namespace('vdiff_gh')
vim.api.nvim_set_hl(0, 'VdiffGhThread', { link = 'DiagnosticVirtualTextWarn', default = true })
vim.api.nvim_set_hl(0, 'VdiffGhThreadResolved', { link = 'Comment', default = true })
local function apply(buf)
  if not vim.api.nvim_buf_is_valid(buf) then return end
  vim.api.nvim_buf_clear_namespace(buf, ns, 0, -1)
  local name = vim.api.nvim_buf_get_name(buf)
  if name == '' then return end
  local count = vim.api.nvim_buf_line_count(buf)
  for path, threads in pairs(data) do
    if name == path or name:sub(-(#path + 1)) == '/' .. path then
      for _, t in ipairs(threads) do
        local hl = t.resolved and 'VdiffGhThreadResolved' or 'VdiffGhThread'
        local lines = {}
        for _, text in ipairs(t.text) do
          table.insert(lines, { { text, hl } })
        end
        pcall(vim.api.nvim_buf_set_extmark, buf, ns, math.min(t.line, count) - 1, 0, {
          virt_lines = lines,
        })
      end
    end
  end
end
local group = vim.api.nvim_create_augroup('vdiff_gh', { clear = true })
vim.api.nvim_create_autocmd({ 'BufEnter', 'BufWinEnter' }, {
  group = group,
  callback = function(ev) apply(ev.buf) end,
})
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_is_loaded(buf) then apply(buf) end
end
"#;

/// The virtual lines shown under `thread`'s anchor: each comment's body,
/// the first line prefixed with its author (replies marked with `↳`), and
/// `[resolved]` on the opening line of a resolved thread. Split with
/// [`str::lines`], so GitHub's `\r\n` line endings never reach nvim.
pub fn thread_virt_lines(thread: &ReviewThread) -> Vec<String> {
    let mut out = Vec::new();
    for (index, comment) in thread.comments.iter().enumerate() {
        let marker = if index == 0 { "" } else { "↳ " };
        let mut lines = comment.body.lines();
        let first = lines.next().unwrap_or("");
        out.push(format!("│ {marker}@{}: {first}", comment.author));
        out.extend(lines.map(|line| format!("│   {line}")));
    }
    if thread.is_resolved {
        if let Some(first) = out.first_mut() {
            first.push_str("  [resolved]");
        }
    }
    out
}

/// Quote `s` as a single-quoted Lua string literal that is safe for any
/// input: ASCII letters, digits, space, and a few inert punctuation marks
/// pass through, and every other byte becomes a three-digit `\ddd` escape.
/// Fixed width means a following digit can never be read as part of the
/// escape, and escaping by byte handles multibyte UTF-8 without decoding.
pub fn lua_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for byte in s.bytes() {
        if byte.is_ascii_alphanumeric() || b" /._-:,;()@#?!+=*&%<>{}~^$|`\"".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("\\{byte:03}"));
        }
    }
    out.push('\'');
    out
}

/// The Lua chunk that places `threads` inline: a `data` table of every
/// thread [`inline_line`] can place (outdated ones are left to the thread
/// list), grouped by path, followed by [`APPLY_LUA`].
pub fn threads_lua(threads: &PrThreads) -> String {
    let mut by_path: std::collections::BTreeMap<&str, Vec<String>> = Default::default();
    for thread in &threads.threads {
        let Some(line) = inline_line(thread) else {
            continue;
        };
        let text: Vec<String> = thread_virt_lines(thread)
            .iter()
            .map(|l| lua_quote(l))
            .collect();
        by_path.entry(&thread.path).or_default().push(format!(
            "{{ line = {line}, resolved = {}, text = {{ {} }} }}",
            thread.is_resolved,
            text.join(", ")
        ));
    }
    let mut lua = String::from("local data = {\n");
    for (path, entries) in by_path {
        lua.push_str(&format!(
            "  [{}] = {{ {} }},\n",
            lua_quote(path),
            entries.join(", ")
        ));
    }
    lua.push_str("}\n");
    lua.push_str(APPLY_LUA);
    lua
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::gh_threads::ThreadComment;

    fn thread(resolved: bool, outdated: bool, comments: &[(&str, &str)]) -> ReviewThread {
        ReviewThread {
            id: "T".to_string(),
            path: "src/a.rs".to_string(),
            line: Some(4),
            is_resolved: resolved,
            is_outdated: outdated,
            comments: comments
                .iter()
                .map(|(author, body)| ThreadComment {
                    author: author.to_string(),
                    body: body.to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn virt_lines_show_author_body_and_replies() {
        let t = thread(
            false,
            false,
            &[("bob", "why?\r\nreally"), ("me", "because")],
        );
        assert_eq!(
            thread_virt_lines(&t),
            vec!["│ @bob: why?", "│   really", "│ ↳ @me: because"]
        );
    }

    #[test]
    fn virt_lines_mark_resolved_threads() {
        let t = thread(true, false, &[("bob", "nit")]);
        assert_eq!(thread_virt_lines(&t), vec!["│ @bob: nit  [resolved]"]);
    }

    #[test]
    fn lua_quote_keeps_plain_text_readable() {
        assert_eq!(lua_quote("src/a.rs line 4"), "'src/a.rs line 4'");
    }

    #[test]
    fn lua_quote_escapes_everything_that_could_end_the_string() {
        let quoted = lua_quote("a'b\\c\nd\re]]\0");
        assert_eq!(quoted, "'a\\039b\\092c\\010d\\013e\\093\\093\\000'");
    }

    #[test]
    fn lua_quote_escapes_multibyte_utf8_bytewise() {
        assert_eq!(lua_quote("é"), "'\\195\\169'");
    }

    #[test]
    fn threads_lua_includes_placeable_threads_only() {
        let threads = PrThreads {
            pr_number: 1,
            head_oid: "h".to_string(),
            threads: vec![
                thread(false, false, &[("bob", "keep me")]),
                thread(false, true, &[("bob", "outdated")]),
            ],
            summaries: vec![],
        };
        let lua = threads_lua(&threads);
        assert!(lua.contains(&lua_quote("src/a.rs")));
        assert!(lua.contains("line = 4"));
        assert!(lua.contains("keep me"));
        assert!(!lua.contains("outdated"));
        assert!(lua.contains("nvim_create_namespace('vdiff_gh')"));
        assert!(lua.contains("BufEnter"));
    }
}
