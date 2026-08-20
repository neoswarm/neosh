# Architecture decisions

One file per decision, recording *why* rather than only *what*. Decisions made before any code was
written are here too, because the reasoning is the part that has to survive contact with the
implementation — and in two cases (0005, 0008) it did not, which is itself recorded.

| # | Decision | Status |
|---|---|---|
| [0001](0001-rust.md) | Rust for the core | Accepted |
| [0002](0002-ratatui-crossterm.md) | ratatui + crossterm for the terminal frontend | Accepted |
| [0003](0003-tokio.md) | tokio as the async runtime | Accepted |
| [0004](0004-typescript-not-lua.md) | TypeScript, not Lua, for plugins | Accepted |
| [0005](0005-deno-core-not-rustyscript.md) | Embed deno_core directly | Accepted (supersedes the original rustyscript choice) |
| [0006](0006-frontend-resolves-layout.md) | The frontend resolves layout | Accepted |
| [0007](0007-marks-on-lines-byte-columns.md) | Extmarks live on lines; columns are byte offsets | Accepted |
| [0008](0008-provider-transports.md) | One event shape, several transports | Accepted (expanded beyond the original plan) |
| [0009](0009-hooks-fail-closed.md) | Blocking hooks fail closed; observers cannot veto | Accepted |
| [0010](0010-mcp-and-acp.md) | MCP and ACP as the RPC door | Accepted (direction) |
| [0011](0011-open-provider-drivers.md) | Provider drivers are open and models are discovered | Accepted |
| [0012](0012-config-is-a-plugin.md) | The user's configuration is a plugin (`init.ts`) | Accepted |
| [0013](0013-config-layers-and-project-trust.md) | Two config layers; project config is trust-gated | Accepted |
| [0014](0014-declared-typed-options.md) | Options are declared, typed, and owned | Accepted |
| [0015](0015-timers-and-reload.md) | Rust-owned timers; reload is teardown plus replay | Accepted |
| [0016](0016-capabilities-in-core-interface-in-plugins.md) | Capabilities in the core, interface in plugins | Accepted |
| [0017](0017-raw-key-capture.md) | A window may claim the keys nothing else wanted | Accepted |
| [0018](0018-generation-outside-the-conversation.md) | Generated names and messages never enter the conversation | Accepted |
| [0019](0019-motion-and-the-visual-language.md) | Motion, colour, and what a terminal is actually for | Accepted |
| [0020](0020-plugin-state-is-not-configuration.md) | Plugin state is a separate store, not configuration | Accepted |
| [0021](0021-motions-are-verbs-the-core-resolves.md) | Motions and edits are verbs resolved by the core | Accepted |
| [0022](0022-credentials-never-cross-the-plugin-boundary.md) | A key is typed into the host, and never crosses a boundary | Accepted |
| [0023](0023-archive-not-delete.md) | Archiving is the everyday verb; deleting is the exception | Accepted |
| [0024](0024-plans-and-keys-are-different-accounts.md) | A plan and an API key are different accounts | Accepted |
| [0025](0025-motion-belongs-to-the-frontend.md) | Text that moves is a property of a highlight group | Accepted |
| [0026](0026-a-block-is-settled-once-it-cannot-change.md) | A markdown block is settled once it cannot change | Accepted |
| [0027](0027-one-driver-for-every-agent-that-speaks-acp.md) | One driver for every agent that speaks ACP | Accepted |
| [0028](0028-the-transcript-is-a-place-you-go.md) | The transcript is a place you go, not a pane you focus | Accepted |
| [0029](0029-a-conversation-carries-its-directory.md) | A conversation carries its directory, and everything follows it | Accepted |
| [0030](0030-a-turn-belongs-to-a-conversation.md) | A turn belongs to a conversation, not to the program | Accepted |
