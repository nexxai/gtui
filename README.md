# gtui - Rust Gmail TUI Client

A high-performance, terminal-based Gmail client built with Rust. Features secure
OAuth2 authentication, local SQLite caching, background synchronization, and a
vim-inspired interface.

## Prerequisites

1. **Google Cloud Project**: You must have a project in the [Google Cloud Console](https://console.cloud.google.com/).
2. **Gmail API**: Enable the Gmail API for your project.
3. **OAuth Credentials**: Create "OAuth client ID" credentials (type: "Desktop app").
4. **Download JSON**: Save the client secret JSON file as `credentials.json` in
   the root of this project.

## Setup & First Run

1. **Build the project**:
   ```bash
   cargo build
   ```
2. **Authenticate**:
   The first time you run the app, it will open your default web browser for
   Google authentication.
   ```bash
    cargo run
   ```
3. **Permissions**:
   The app will request broad Gmail permissions to sync labels and send/read emails.
   The authentication token will be securely stored and protected in your OS's
   native credential store (e.g., Keychain on macOS, Credential Store on
   Windows, etc.)

## Configuration

You can customize the application behavior by editing `settings.toml` in the
project root.

### Default Keybindings

| Action                  | Keys                   |
| :---------------------- | :--------------------- |
| **Quit**                | `q`                    |
| **Next Panel**          | `l`, `Right`, `Tab`    |
| **Previous Panel**      | `h`, `Left`, `BackTab` |
| **Move Up**             | `k`, `Up`              |
| **Move Down**           | `j`, `Down`            |
| **Mark as Read**        | `Space`                |
| **New Message**         | `n`                    |
| **Reply**               | `r`                    |
| **Delete**              | `Backspace`, `d`       |
| **Archive**             | `a`                    |
| **Undo Delete/Archive** | `u`                    |

### Customizing Keybindings

Edit the `[keybindings]` section in `settings.toml`. You can provide multiple
keys as an array of strings:

```toml
[keybindings]
next_panel = ["l", "Right", "Tab"]
mark_read = [" ", "x"]
```

### Signatures

You can set automatic signatures for new messages and replies:

```toml
[signatures]
new_message = """
--
Sent from gtui"""

reply = """

Best,
Your Name"""
```

## Features

- **Thread Grouping**: Messages are grouped by thread ID in the list, showing
  one entry per conversation.
- **Reverse Chronological Sorting**: Most recent threads and messages appear at
  the top.
- **Full-Width Layout**: Enhanced UI with bordered blocks for clarity.
- **Popup Composition**: Compose and reply in a focused centered overlay (the
  **Composition Panel**).
- **CC/BCC Support**: Use `Ctrl+B` while composing to toggle optional CC and
  BCC fields.
- **Automated Quoting**: Replies automatically include the full body of the
  original message.
- **Background Sync**: Keeps your local cache updated with the latest emails.

## Troubleshooting

- **Authentication Failed**: Ensure `credentials.json` is present and correctly
  configured in Google Cloud.
- **Database Errors**: If the local cache becomes corrupted, you can safely
  delete `gtui.db` and the app will re-sync on next startup.
- **Keychain Access**: Ensure the app has permission to access the macOS Keychain
  if prompted.
