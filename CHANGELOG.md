# Changelog

Track new features, improvements, and bug fixes in **Antimetro** for Google Antigravity App 2.0.

---

## v1.4.1

**November 27, 2026**

### Instant Cross-Chat Quota Sync, Idle Reset Auto-Refresh, Active Model Highlight, and Cleaner Card Ergonomics

This maintenance release keeps model quota rings instantaneously synchronized across all open chats, automatically refreshes expired quota buckets even while idle, subtly highlights which quota pool (`Gemini` or `Other`) the active chat is using, and removes visual tooltip clutter from self-explanatory card icons. It includes **2 improvements** and **2 fixes**.

### Improvements (2)

- **Active quota pool highlight:** The **Gemini** or **Other** label above the quota rings now subtly highlights to match the model family currently responding in the open conversation, so you can tell at a glance which quota pool your active chat draws from.
- **Quieter Notes & Search cards:** Removed redundant hover tooltips on self-explanatory card header icons (`Pin`, `Copy`, `Delete`, `Confirm`, `Cancel`) while keeping `aria-label` accessibility and keeping helpful tooltips everywhere they add information (quota reset countdowns, `Undo`, search scope, Auto-compact handle, and `About` link URLs).

### Fixes (2)

- **Instant quota sync across all open chats:** Quota updates are now broadcast immediately across all open Antimetro side-panel tabs via `BroadcastChannel` (with zero extra network calls) and refreshed from the local in-memory cache on conversation switch and window focus, eliminating stale quota numbers when switching between chats.
- **Idle quota reset auto-refresh & `1.5s` post-turn settle:** When any visible quota bucket's `reset_time` passes while Antigravity is idle, the 60-second background check now triggers a single `forceRefresh` so the ring resets to `100%` automatically without needing to send a prompt; adjusted post-turn refresh delay from `1s` to `1.5s` so server-side quota accounting settles reliably before the final read.

---

## v1.4.0

**November 26, 2026**

### 1-Click Jump to Chat from Search, Update Notifications, Color & Quota Refinement, and Notes Polish

This release turns cross-chat **Search** into an instant conversation navigator, adds a lightweight once-a-day update indicator on **About**, refines the amber warning palette, button proportions, and live quota refresh timing, and eliminates **Notes** header focus flicker, layout shifts, and stuck compaction states on quota errors. It includes **6 improvements** and **6 fixes**.

### Improvements (6)

- **1-click conversation jump in Search:** Clicking any conversation title (`↗`) in global **Search** (`All`) now opens that conversation directly in Antigravity while keeping the Antimetro side panel open alongside it.
- **Quiet daily update indicator:** Antimetro now checks GitHub Releases at most once every 24 hours (with zero user data sent and local state persisted across restarts), displaying a subtle blue accent dot on **About** (with a gentle double pulse once per minute) and a direct download link inside the popover whenever a newer release is available.
- **Refined About drawer & unified `28px` button proportions:** Added smooth slide-open / slide-close animation to **About**, added a dedicated **Feedback** button linking straight to GitHub Issues alongside **GitHub** and a full-width **♥ Support / Donate** button, added destination URL tooltips on hover for all external links, and unified all action buttons (`Compact now`, `Undo`, `About`, and `About` links) to a crisp `28px` height matching the Auto-compact threshold input.
- **Cleaner warm amber-orange indicator color:** Tuned `--am-warn1` in both light (`#ea680c`) and dark (`#fb8c24`) themes and unified progress bar saturation with the quota rings for crisp contrast without muddy yellow-brown tones.
- **Faster post-turn quota refresh (`1s`):** Reduced the post-turn quota refresh delay from `2s` to `1s` (with an `800ms` server deduplication window) so final session quota rings settle faster right after a reply finishes.
- **Antigravity 2.35.2 compatibility & manifest sync:** Updated compatibility metadata for **Antigravity App 2.0 (v2.35.2)** and refreshed `plugin.json` / `sidecar.json` descriptions across all installers.

### Fixes (6)

- **Fixed `Compact now` button getting stuck on `Working...` after a quota or model error:** When `/compact` fails immediately (e.g., quota exhausted), the button now resets cleanly back to `Compact now` as soon as the conversation returns to idle (or on the pending timeout), and `ERROR_MESSAGE` transcript steps now explicitly clear `compact_in_flight`.
- **Eliminated Copy icon & border flicker in Notes:** Prevented card header action buttons (`Pin`, `Copy`, `Delete`, `Confirm`, `Cancel`) from stealing or briefly triggering focus, tying `Copy` visibility and the active card border strictly to active text editing (including across animated `Pin` reordering).
- **Fixed horizontal layout shift when opening About:** Hidden the root document scrollbar while preserving wheel scrolling on shorter viewports so opening **About** never narrows cards or shifts the right margin.
- **Fixed missed mid-turn quota updates inside the 15s throttle window:** Token increases occurring during the 15-second cooldown now schedule a deferred refresh at the end of the window (and track brand-new chat creation on the first step) instead of dropping the update until a later step.
- **Unified Notes & messages ranking in global Search:** Combined note matches and transcript matches now sort together by match quality tier (`exact phrase` → `close window` → `scattered terms`) so scattered words in a note never outrank an exact phrase match in a conversation.
- **Accurate `1%` floor for non-zero quota:** Remaining quota fractions below `0.5%` (e.g. `0.3%`) now display `1%` instead of rounding down to `0%`, reserving `0%` strictly for fully exhausted buckets.

---

## v1.3.0

**November 25, 2026**

### Model Quota Rings, Automatic Orphan Cleanup, Overlay Scroll Rail, and Notes Polish

This release brings live **Gemini** and **Other** (Claude/GPT) weekly & 5-hour quota rings right under the token meter, automatic cleanup of orphaned plugin data when chats are deleted, a zero-shift overlay scroll rail, a cleaner compact date bar, and a critical fix for note card expansion jumping. It includes **7 improvements** and **2 fixes**.

### Improvements (7)

- Added a live **Model Quota** panel directly below the token meter showing **Gemini** and **Other** weekly (`W`) and 5-hour (`5h`) remaining quota with symmetric color-coded rings (`66–100%` green, `31–65%` amber, `1–30%` red), left-aligned percentages, and concise `Resets in ...` tooltips. Refreshes live on real model steps during active work (throttled to at most once every `15s`), `2s` after a turn finishes, and passively from cache every `60s` while idle.
- Added safe automatic cleanup of orphaned per-chat data (`notes/<id>.json`, `.json.bak`, and stored chat settings/cutoffs) when conversations are permanently deleted in Antigravity, while always preserving active, archived, and draft chats.
- Replaced the left dot-rail scrollbar with a sleek `3px` auto-hiding overlay scroll rail in the right outer margin (and matched inner card scrollbars to `3px` at the same soft opacity), eliminating horizontal layout shifts and text reflows while scrolling.
- Streamlined the chat metadata card into a calmer single-row date layout (`First 22 Nov, 11:36` / `Last 24 Nov, 16:18`) to give more vertical room to quotas and notes.
- Refined **Notes** card ergonomics: swapped `Pin` and `Copy` icon order (`Pin` first) and made `Copy` appear only when a card is focused to prevent accidental clicks and reduce visual clutter.
- A single non-empty note card now cleanly collapses to 5 lines when focus leaves, while an empty chat's initial note card stays stretched to the bottom of the panel until you start writing and click away.
- Updated compatibility badge and metadata for **Antigravity App 2.0 (v2.35.1)**.

### Fixes (2)

- Fixed a bug where clicking any collapsed note card caused the card list and textarea to jump abruptly to the middle, hiding the card header above the viewport (caused by `.cards-viewport` missing `position: relative` for `offsetTop` calculations).
- Fixed subtle inertia cutoff and stutter when scrolling inside solo or expanded note cards by adding subpixel scroll accumulators and strict edge handoff checks in the unified wheel engine.

---

## v1.2.0

**November 24, 2026**

### Draggable Auto-Compact Pin, Card-First Notes, Archived & Notes Search, and Seamless Scroll

This release upgrades **Antimetro** with a draggable circular Auto-compact threshold handle right on the token meter, cross-chat search that now includes archived chats (up to `500`) and saved notes from every conversation, a streamlined card-first **Notes** & **Search** header with smooth drawer animations, unified nested wheel scrolling, and live Language Server status accuracy. It includes **8 improvements** and **3 fixes**.

### Improvements (8)

- Replaced the static Auto-compact notch on the token meter with a draggable circular handle (`10K` snap increments from `10K` to `500K`), complete with live tooltip readout while dragging, keyboard arrow adjustments, and `Escape` to cancel.
- Expanded global **Search** (`All` by default) from `200` to `500` recent local chats, including archived conversations labeled with a `· archived` badge and a faster `5s` conversation title refresh cache.
- **Search** now matches your per-chat **Notes** alongside conversation messages (tagged with a `Note` badge), covering both live unsaved notes in the current chat and saved notes across all other chats.
- Redesigned **Notes** into a cleaner card-first flow without a separate top scratchpad: when a chat has `0` or `1` note, that solo card stretches cleanly to the bottom of the panel and stays expanded until you add a second card, with a per-card save indicator right next to `Pin` and a blue accent border highlighting whichever note or search card is currently active.
- Streamlined the `Notes` / `Search` header with crisp larger action icons (`+` square-plus on the left to add a card, scan-search toggle on the right to filter strictly to `This chat`), a smooth bottom-pinned slide drawer for the search box, an inline amber match counter inside the search input next to a larger clear `×` button, and balanced card icon spacing.
- Unified wheel scrolling across expanded cards and the outer card list onto a single smooth glide curve, eliminating speed overlap, discrete jumps, and Chromium's boundary-latching pause when scrolling past the end of a long card.
- Expanding a long **Search** result now locks the best-matching phrase (`mark.is-focus`) to the center of the card from the very first frame of expansion instead of jumping after a delay.
- Animated the left overflow dot-rail scrollbar so it slides smoothly in and out without abruptly shifting card widths, tracking card expand/collapse and list changes continuously.

### Fixes (3)

- Integrated live `GetAllCascadeTrajectories` status alongside transcript background-task tracking so completed, canceled, or idle sessions no longer leave the rainbow progress bar spinning or keep **Compact now** / **Undo** locked, while hiding **Undo** during active background work and resetting it as soon as a new step runs after compaction.
- Ensured **Auto-compact** waits cleanly for the active agent turn and background tasks to finish before triggering compaction, preventing mid-turn interruptions while files are being edited.
- Reserved fixed layout space for the per-card save indicator so card icons never jitter during autosave, and excluded `__draft__` notes from server-side `All` scans so unsaved notes in brand-new chats are never duplicated in search results.

---

## v1.1.0

**November 23, 2026**

### Pinned Notes, All-Chats Search, Smart Ranking, and Native Tooltips

This release adds animated pin-to-top note cards, cross-chat search across your local history, smarter multi-word search ranking that prioritizes exact phrases and close-proximity matches, reverse-flow compaction animation, and Antigravity-style pill tooltips. It includes **6 improvements**.

### Improvements (6)

- You can now **Pin** important note cards in **Notes** so they stay anchored at the top of the list even when new cards are created (`pinned` state persisted per chat in `notes/<conversationId>.json` with full backward compatibility), with flicker-free in-place accent border transitions and smooth animated reordering when moving cards up or down.
- Added a **This** / **All** scope toggle inside **Search** to search prompts, replies, and compaction summaries either in the active conversation or across all recent local chats (up to `200` most recent transcripts), showing conversation titles and marking the active chat.
- Upgraded multi-word **Search** with proximity-aware ranking and best-window snippets: exact phrases rank first, followed by matches where all search terms appear close together (within `160` characters), then scattered matches—sorted newest-first within each tier—while snippets and expanded excerpts automatically center on the tightest window containing all matched words.
- The animated rainbow activity bar now reverses direction (flowing right-to-left) during compaction so it visually distinguishes context shrinking from normal generation.
- Replaced native browser tooltips across the side panel with crisp Antigravity-style pill tooltips that follow the active light/dark theme, stay within narrow panel bounds, and show truncated chat titles in `All` search only when needed.
- Improved **Search** responsiveness by immediately cancelling in-flight `All`-chats disk scans whenever you change the query or switch back to `This`, expanded the conversation title response cap to `8 MB`, and updated `plugin.json` version metadata, installers, and unit tests for `v1.1.0`.

---

## v1.0.1

**November 23, 2026**

### Multi-word Search, Notes Ergonomics, and Core Reliability

This release brings multi-word out-of-order search, keyboard shortcuts, and visual card truncation cues, along with Linux `musl` portability and core reliability fixes. It includes **8 improvements** and **11 fixes**.

### Improvements (8)

- **Search** now matches multiple words in any order across a message (or exact phrases when wrapped in quotes `"..."`), highlighting every matched term.
- You can now press `Ctrl+Enter` (`Cmd+Enter` on macOS) in the top **Notes** field to save the current note as a card below.
- You can now press `Ctrl+F` (`Cmd+F` on macOS) anywhere inside the Antimetro panel to switch directly to **Search** and focus the search box.
- Collapsed 5-line note cards now show a subtle bottom fade when a card contains additional lines of text inside.
- Added visual status indicator and explanatory tooltips when a conversation reaches the per-chat note limit (`200` cards or `256 KB`).
- Improved transcript search and live polling efficiency by filtering search matches on the fly without holding non-matching messages in memory and avoiding per-poll step vector clones.
- Linux release binaries are now statically linked (`x86_64-unknown-linux-musl`) so they run out of the box on any Linux distribution regardless of the system `glibc` version.
- Added smooth layout wrapping for narrow side panels, full `plugin.json` metadata (`version`, `author`, `homepage`, `repository`), and hardened loopback/header validation.

### Fixes (11)

- Fixed an issue where creating the first note card with `+` or returning to a conversation could render collapsed note cards as a single line (`22px`) instead of up to 5 lines.
- Fixed an issue where note and search card heights did not recalculate when resizing the side panel horizontally or when loading a chat while the **Search** tab was active.
- Fixed an issue where clicking **Undo** right after an automatic compaction could re-trigger **Auto-compact** 1 second later.
- Fixed an issue where a transient error during **Auto-compact** in a conversation with no prior checkpoints could prevent **Auto-compact** from firing on subsequent turns.
- Fixed an issue where clicking dots in the left dot-rail scrollbar could fail to scroll after switching chats, deleting/adding cards, or running a new search with the same dot count.
- Fixed an issue where canceled (`Stop`) or errored agent steps could leave the progress bar animating in the busy state and keep **Compact now** / **Undo** disabled.
- Fixed an issue where `/compact` commands carrying large metadata wrappers (`> 1 KB`) or preceded by `<CONTEXT_SUMMARY>` could be counted as regular user messages.
- Fixed an issue where clicking directly from an expanded top note into a lower card could interrupt the top card's smooth collapse animation.
- Fixed an issue on Windows where user profile paths containing apostrophes could break `Install.bat`, and removed the 1-second gap after `taskkill` so running Antigravity instances cannot lock `antimetro.exe` during updates or uninstallation.
- Fixed an issue where theme auto-detection could miss light or dark mode if Antigravity provides `--background` as a computed `rgb(...)` value rather than a hex string.
- Fixed an issue where exceeding `500` stored chats evicted a random chat instead of the oldest entry, ensured draft notes are deleted only after successful promotion to a new chat, and added `.json.bak` backups before resetting corrupted note or settings files.

---

## v1.0.0

**November 22, 2026**

### Initial Release

First public release of **Antimetro** for Antigravity App 2.0 (tested on `v2.34.0`), built in pure Rust with zero external crates (`0` dependencies).

- Live context window token counter (`cache_read_tokens + input_tokens`) with 3 semantic health zones (`0–200K` green, `200K–300K` amber, `300K–512K` red) and animated rainbow flow while the model is busy.
- Full-session token delta (`+N / session` across tool calls and `-N / session` freed tokens right after compaction).
- 1-click **Compact now** with active-turn and post-compaction lock plus 1-click **Undo** (`RevertToCascadeStep` with `conversationOnly: true`) and native Antigravity completion sound.
- Per-chat **Auto-compact** (`Auto-compact after [ 250 ] K`) remembered individually for each conversation.
- Per-chat **Notes** with Zed-style auto-save, standalone 5-line block cards (`+`), smooth click-to-expand editing, left dot-rail scrollbar, two-step delete confirmation, and empty-chat draft promotion.
- In-chat **Search** across user prompts, model replies, and compaction summaries, including pre-compaction history (`before compact`).
