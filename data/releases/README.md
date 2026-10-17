# Release summaries

After an update, the app shows a short "What's new" summary inside its
"Update complete" notification. Summaries are not bundled with the app; it
fetches them from this directory on `main`:

```
https://raw.githubusercontent.com/vertopolkaLF/codex-minibar/main/data/releases/<version>/whats-new.<locale>.md
```

## Layout

```
data/releases/
  1.2.0/
    whats-new.en.md      # required unless the release has no summary
    whats-new.ru.md      # optional; missing locales fall back to English
    hero.webp            # images referenced by the summaries
```

Locales: `en`, `ru`, `pt-BR`, `es`, `zh-CN`, `zh-TW`, `ja`, `de`, `fr`.

The Release workflow fails early when `whats-new.en.md` is missing for the
version being released; uncheck its `whats_new` input for a release that
deliberately has none (for example a patch release). Merge the summary into
`main` before publishing the release: the app reads it from `main`, and a
summary that is still missing three days after an update is never shown.

## Writing a summary

Keep it short: the notification card is 360 px wide and scrolls past about
360 px of content. Link to the full release notes instead of repeating them;
the card already has a "Full release notes" button.

Supported Markdown:

- `#`, `##`, `###` headings (all drawn as one bold line)
- paragraphs, bullet and numbered lists, nested lists
- `**bold**`, `*italic*`, `~~strikethrough~~`, `` `code` ``, code blocks
- `[links](https://...)`, HTTPS only
- `![alt](image.webp)` and `---`

Raw HTML tags are dropped (their text stays). Images must be relative paths
inside the version directory (no `..`, no URLs). PNG, JPEG, WebP and GIF
(first frame) work; each file must be under 3 MB, at most 6 per summary.
Images are scaled to the card width and at most 200 px tall.

## Example

```markdown
**Projects in Usage.** See spend per repository and folder.

![Usage by project](projects.webp)

- Usage tab groups sessions by project
- Faster startup when history is large
```
