# Changelog

All notable changes to pi-rust are documented here. Versions follow [Semantic Versioning](https://semver.org/); before 1.0, breaking changes bump the minor version.

## [0.1.7] - 2026-10-14

### Added

- **env:** port the pi-env client as crate pi-env with the conformance suite ([d9fb005](https://github.com/yannelli/pi-rust/commit/d9fb0053029673088b2d28092388c54ac10c2567))
- **env:** add the pi-env daemon as crate pi-env-daemon ([cef637d](https://github.com/yannelli/pi-rust/commit/cef637d49e2cbac73fca191a5925c20709614cf8))
- **durable:** port watchEvents and the harness-events tests ([b519ada](https://github.com/yannelli/pi-rust/commit/b519adacb197bfb7ad72cb31c3110a1c138cf877))
- **durable:** port the task graph view and harness-task-graph tests ([0393841](https://github.com/yannelli/pi-rust/commit/03938414a537a2f484fa234e91eaf6f8d8f336d2))
- **durable:** port coding tools read, write, edit, bash, and powershell ([2d4ac92](https://github.com/yannelli/pi-rust/commit/2d4ac929e4e6ab5067ce0d4e6445a4abe76516a5))
- **durable:** add Harness, Conversation handles, and conversation views ([81e1086](https://github.com/yannelli/pi-rust/commit/81e1086207e9395552f466931d191d16437c1f26))
- **durable:** wire the harness module and built-in entry kinds ([ae0dcef](https://github.com/yannelli/pi-rust/commit/ae0dcef2fea0a98c599c5f128c9a1bd5d26d2a52))
- **durable:** add harness scheduler runtime, generation, tool, compaction, and submissions (not yet wired) ([2260159](https://github.com/yannelli/pi-rust/commit/2260159eb99188895938d3078adff62abb37b894))
- **durable:** add harness data types, documents, context, prompt, and scheduler core (not yet wired) ([f3e736d](https://github.com/yannelli/pi-rust/commit/f3e736d2308a96f3515e9f9a4bedfc2787c0ba71))

### Fixed

- **durable:** report a file above a recursive mkdir as not_directory, and list every seed's first difference ([9abf062](https://github.com/yannelli/pi-rust/commit/9abf0623c0952104830847bdc480aca5dd140e0a))
- **durable:** refuse moving a directory into itself on Windows, and accept a late-join output that ends with the tool ([511cce0](https://github.com/yannelli/pi-rust/commit/511cce0bec2469025f7f7e4487858f3b6bc7a768))
- **durable:** refuse directory renames onto existing paths on Windows, as MoveFileExW does ([57da3b0](https://github.com/yannelli/pi-rust/commit/57da3b0833e908d7fdfad8b2f2f92521b8b7a9e2))
- **durable:** let an empty append to a directory succeed on Windows, as libuv does ([9008dec](https://github.com/yannelli/pi-rust/commit/9008dece83d6e8787244158591b3e391e095bab5))
- **durable:** map Windows errors through libuv's table, and refuse directory reads as Node does ([466ea88](https://github.com/yannelli/pi-rust/commit/466ea88dd407a7f1295e3dcaa1e495eba689e65b))
- **durable:** report writes and truncation of a directory on Windows as Node does ([6674980](https://github.com/yannelli/pi-rust/commit/6674980234089b6c7f49a783576a839a5f0b59cd))
- **durable:** match Node for directory listing order and non-recursive directory removal ([5058d78](https://github.com/yannelli/pi-rust/commit/5058d78e3d8257f38365a40c7ca3a28b2daacdb0))

## [0.1.6] - 2026-10-13

### Added

- **durable:** restore pi-durable ([5d236f1](https://github.com/yannelli/pi-rust/commit/5d236f143dda37172dfef20c8ce23151acb92dbd))

### Fixed

- **durable:** use the NTFS file index as the Windows watch identity ([49582d9](https://github.com/yannelli/pi-rust/commit/49582d938951a6e6d6f407521736869176221fc1))
- **durable:** map FSEvents real paths to the watched spelling, and fix Windows test expectations ([677804b](https://github.com/yannelli/pi-rust/commit/677804b93a37cef38801565da85abe785b554f3f))

## [0.1.5] - 2026-10-13

### Added

- **durable:** add JsonlStorage over the FileSystem capability ([e3ed422](https://github.com/yannelli/pi-rust/commit/e3ed422ceae1b3a96af5c5d0d2dea7669ffff456))
- **durable:** port the Session kernel, document definitions, and typed entries ([24a50b9](https://github.com/yannelli/pi-rust/commit/24a50b9855214eb89ce857d301750797559fd1db))
- **durable:** add the image processor on the image crate with EXIF orientation ([3173ba3](https://github.com/yannelli/pi-rust/commit/3173ba345d075ea3cfb92b56534efd7a076f2f5c))
- **durable:** add SqliteStorage over a portable database facade with bundled SQLite ([0188ba8](https://github.com/yannelli/pi-rust/commit/0188ba8e3a1af914b234681b7e251b8d584e374c))
- **durable:** add local file watcher and the execution env conformance suite ([9dbb321](https://github.com/yannelli/pi-rust/commit/9dbb3218bc47c69a5d0dfcf6bb49e1e12aa5b10f))
- **durable:** add LocalExecutionEnv shell with spills, timeouts, and process-tree kills ([295f7e2](https://github.com/yannelli/pi-rust/commit/295f7e2886a28109904ad89aab1bd6b6e4fb9314))
- **client:** port pi-client with Unix transport, discovery, and an in-process server test ([a4e3de8](https://github.com/yannelli/pi-rust/commit/a4e3de80c1c78c55083fc6050cf9bd303970a5cb))
- **durable:** add LocalExecutionEnv file system, decoders, line scanner, and truncation ([44fc2a9](https://github.com/yannelli/pi-rust/commit/44fc2a9bc004a66dcf554031fe638497bdb87cc6))
- **durable:** add Models trait, registry-backed model access, faux provider, and uuidv7 helpers ([dc6a23c](https://github.com/yannelli/pi-rust/commit/dc6a23cbb9e906b8f3417453981aad75ce435fd8))
- **durable:** add MemoryStorage and the storage conformance suite ([af55095](https://github.com/yannelli/pi-rust/commit/af550950001563bd0078a5927f189e48408ceb47))
- **server:** port pi-server router, Unix listener, and test harness ([c0e0a80](https://github.com/yannelli/pi-rust/commit/c0e0a80e93105dada4df38d8dcd57b5b0de01aca))
- **protocol:** port pi-protocol CBOR codec, framing, and envelopes ([2c6a270](https://github.com/yannelli/pi-rust/commit/2c6a2706ca489c0fe38367c46e9d8e33e049d8cd))
- **durable:** add testing module root ([da743b7](https://github.com/yannelli/pi-rust/commit/da743b7ec860812ee8fbe6c416eb9097c7ca1b4d))
- **durable:** add pi-durable crate with records, storage contract, and execution env interface ([a8c0a4b](https://github.com/yannelli/pi-rust/commit/a8c0a4b2b44d173f7f17316343e45f579581fe14))

### Fixed

- hold pi-durable for the next release ([d9c9038](https://github.com/yannelli/pi-rust/commit/d9c903881304ed0d80fbea781ca13be1d734366a))
- **client:** validate service calls before sending them ([0b31baf](https://github.com/yannelli/pi-rust/commit/0b31bafc0ad9a45538c7bf61dbf21f91632a4cbf))

## [0.1.4] - 2026-10-13

### Added

- **chord:** bundle facets with oxc and load them through QuickJS ([114d294](https://github.com/yannelli/pi-rust/commit/114d294bcff0e4bdba05298977a90aa6eeb54e5b))
- **chord:** read, check, and load facet bundle manifests and artifacts ([e7b5465](https://github.com/yannelli/pi-rust/commit/e7b546577656680613d0391537013ee1dbb72d07))
- **chord:** add pi-chord crate with JSON, delta, state, services, and facets ([8d2f451](https://github.com/yannelli/pi-rust/commit/8d2f4519b80fa8f431f89e08867f8acda2db5c18))
- **interactive:** ambient-credential login dialog, panic crash kind, and Termux helper tool warnings ([57c8aa9](https://github.com/yannelli/pi-rust/commit/57c8aa9a0af76b09cfc56c56fdb3e3b35ddfe120))
- **models:** compose extension providers like upstream, route queued input through input handlers, keep extension stacks ([7811114](https://github.com/yannelli/pi-rust/commit/781111404df05baafbdff9e56be437840a4303d9))
- **models:** validate models.json against the upstream schema with upstream error texts ([4a269e3](https://github.com/yannelli/pi-rust/commit/4a269e3981212d1af241b9678a07f6cff96f616c))

### Fixed

- **tools:** signal the process group directly on cancel, and make the bash cancel tests wait for progress ([8922b6c](https://github.com/yannelli/pi-rust/commit/8922b6ccbb5a44840830b4736ae4f81a2aa187b6))
- **chord:** drop settled tracker drafts and keep draft key order ([458e0a3](https://github.com/yannelli/pi-rust/commit/458e0a3703e1cb6b439ba5c0619a7fee9709dde4))
- **ai:** await payload hooks instead of blocking, and drop the 30s before_provider_request limit ([bf0b28e](https://github.com/yannelli/pi-rust/commit/bf0b28e8a16ddff61e0dbb97d719a6c4451daf0d))
- **session:** abort before session_shutdown, await the UI detach, and pass trust UIs to project_trust handlers ([cdf69fd](https://github.com/yannelli/pi-rust/commit/cdf69fd5c7cbe82e7c907bee3021ba5b2a300aa8))
- **cli:** match upstream startup timings, help branding, stdout takeover, and env overlay ([462792a](https://github.com/yannelli/pi-rust/commit/462792acd02d4b0e66c1925e16b25d0faf34251c))

## [0.1.3] - 2026-10-13

### Added

- **embedded:** add pi-embedded crate with in-process sessions, host adapters, and a C ABI for native hosts ([68f1bca](https://github.com/yannelli/pi-rust/commit/68f1bca2ebf93c77b724a6be27ff0dd9a1066b8c))
- **telemetry:** add pi-telemetry crate and thread telemetry context through pi-ai options ([96a0459](https://github.com/yannelli/pi-rust/commit/96a045902ad11358a3f730dd427d71ee0a63642d))

## [0.1.2] - 2026-10-13

### Fixed

- **agent:** await async listeners for each event before the loop continues ([e8b7168](https://github.com/yannelli/pi-rust/commit/e8b7168c392ee277d96e74bcb1a08e9b3289d9fc))

## [0.1.1] - 2026-10-13

### Added

- **interactive:** share concurrent catalog refreshes and name suspect extensions after a fatal error ([19bda11](https://github.com/yannelli/pi-rust/commit/19bda115d406d3af6b0d993537ad7bb3bd1bc0d6))
- **interactive:** close the interactive parity gaps ([2de194e](https://github.com/yannelli/pi-rust/commit/2de194e4d15caad0d93f5be957251e2ef09e5fa2))
- **extensions:** interactive extension UI, entry renderers, startup project trust, and tests ([d6ce03a](https://github.com/yannelli/pi-rust/commit/d6ce03ae2b366955926924d8023a6cfad747b2ee))
- **startup:** wire migrations, startup dialogs, signals, timings, and benchmark into main ([6681e24](https://github.com/yannelli/pi-rust/commit/6681e2479f0dd87f21625958d9dd14772fdf062d))
- **startup:** migrations, stop signal handling, startup dialogs, and crash terminal restore ([ce6055f](https://github.com/yannelli/pi-rust/commit/ce6055f824e4a91a5b2fba2c3ed85e378127c4a5))
- **ai:** pi-ai dev CLI with login and list ([6fe0006](https://github.com/yannelli/pi-rust/commit/6fe000610848debde4ea9642411d6c5ce2fcc3d5))
- **ai:** assistant message frame encoder and reducer ([aff2d4e](https://github.com/yannelli/pi-rust/commit/aff2d4e064fb459d82bc7e77bb592d6491be9780))
- **agent:** async event subscribers with the run's abort signal and an idle barrier ([1a94bee](https://github.com/yannelli/pi-rust/commit/1a94bee4939adbfa3e73939127bcc3d61489cb9a))
- **sdk:** tool options in CreateAgentSessionOptions and per-session bash environment ([5716371](https://github.com/yannelli/pi-rust/commit/571637136a48c326c34d77f8213aa1edc72af5fe))
- **tools:** pluggable read, write, edit, grep, find and ls backends ([e07f73c](https://github.com/yannelli/pi-rust/commit/e07f73ca028ab6d4a72d2c29f3e42b998714386b))
- **extensions:** RPC and ACP extension host, extension CLI flags in help ([bd194f0](https://github.com/yannelli/pi-rust/commit/bd194f0e005f495d4860c147e1a45f92273909b4))
- **acp:** end-to-end ACP tests, edit snapshots from the event stream, extension host binding ([5ec8bf7](https://github.com/yannelli/pi-rust/commit/5ec8bf792d35983de99168dbf2fcc29056c9bdb9))
- **acp:** Agent Client Protocol mode (pi --mode acp), ported from pi-acp ([c7a6b22](https://github.com/yannelli/pi-rust/commit/c7a6b22a19502a1e69f0f3f14a51d923ade619b7))
- **tui:** segment CJK and Thai text into dictionary words ([030bd0e](https://github.com/yannelli/pi-rust/commit/030bd0e821e41929af25ae8eff29c2e12bfd1a95))
- **ai:** support external account and impersonated ADC credentials for Vertex ([10f75d5](https://github.com/yannelli/pi-rust/commit/10f75d59d23b66ed8581a73ff234ed4ec052e438))
- **ai:** BedrockOptions MFA code hook reaches the AWS credential chain ([229c20c](https://github.com/yannelli/pi-rust/commit/229c20ce85ebfa8b0bf2146701c969538fab5c8f))
- **mcp:** extension-registered servers, overridden notices, connection ids, unsubscribe, server name in sign-in prompt ([4aa77d8](https://github.com/yannelli/pi-rust/commit/4aa77d8030e9f67807b14a3ecd37ef4ae3f71c5b))
- **ai:** refresh AWS SSO session tokens and prompt for MFA codes in assume-role profiles ([57c4e20](https://github.com/yannelli/pi-rust/commit/57c4e2046537a3269ca09170bb8457a26676ebe4))
- **ai:** tunnel the Codex WebSocket through the configured HTTP(S) proxy ([c46253e](https://github.com/yannelli/pi-rust/commit/c46253e03ac0e68c6e035d75fc2658a0b6f6d1aa))
- **ai:** per-call Vertex, Azure, Bedrock, reasoning summary, and pi-messages debug options ([2a8ecf3](https://github.com/yannelli/pi-rust/commit/2a8ecf35dc5d8200cf7b71eae6b2e65906965584))
- **js:** CommonJS and extension probing for module sources registered in memory ([5b189e3](https://github.com/yannelli/pi-rust/commit/5b189e341dff5cacd115b33557a7f40f12c5b157))
- **coding-agent:** wire pi update, crash hook, stdout guard, shell resolution, and startup timings ([d15499e](https://github.com/yannelli/pi-rust/commit/d15499e4fb859dbb6ad930c468dc6905760fd421))
- **js:** load module sources registered in memory, and webcrypto subtle.digest ([3d7d8b8](https://github.com/yannelli/pi-rust/commit/3d7d8b886853f757de7e1a73fb44e77c17fb4f36))
- **agent:** await an on_event hook for every loop event, with message_end replacement and ordered tool updates ([a31cc07](https://github.com/yannelli/pi-rust/commit/a31cc078f4e013442319ecc4beb3fbcf2591a719))
- **interactive:** full-screen interactive mode in place of the line REPL ([a0b6d7e](https://github.com/yannelli/pi-rust/commit/a0b6d7e5a31a91de9715f4add17a6f22faa55855))
- **coding-agent:** /llama session methods, llama.cpp models in the registry, and catalog startup tests ([641fe32](https://github.com/yannelli/pi-rust/commit/641fe32274f6cedc1a2af2fe6e04b69034e1555b))
- **rpc:** RPC mode, extension UI subprotocol, and RpcClient ([8a3a4c4](https://github.com/yannelli/pi-rust/commit/8a3a4c4a1ab4b5e4386f63ec634983b76a3e19b2))
- **coding-agent:** /mcp manager view over McpManager or the session ([13b5a27](https://github.com/yannelli/pi-rust/commit/13b5a27a3c134d5d310142c57ac0a171423bc70b))
- **coding-agent:** virtual model routing, cache warming, cache-miss notices, and dynamic catalogs in sessions ([2b679ff](https://github.com/yannelli/pi-rust/commit/2b679ffb45cf92cc187e106ea0de142455afc4a4))
- **coding-agent:** /llama view with Hugging Face search, progress, and LlamaUi bridge ([513b167](https://github.com/yannelli/pi-rust/commit/513b167b2d36c0671850d3c10bb7a95d4aa4db90))
- **js:** add assert, tty, constants, diagnostics_channel, and stream/consumers; run nextTick before microtasks ([95b49e5](https://github.com/yannelli/pi-rust/commit/95b49e5ba889dcecf381a30e6093d8a1cc38531f))
- **js:** add fetch, Headers, Request, Response, FormData, and web streams ([61df256](https://github.com/yannelli/pi-rust/commit/61df2562ebf06dded52faa7b287074d6b8e76dc6))
- **coding-agent:** re-export every dialog component at the module root ([f2477ad](https://github.com/yannelli/pi-rust/commit/f2477ad570156c57788697d8fd0353207ab73272))
- **js:** add child_process on tokio processes ([0953a24](https://github.com/yannelli/pi-rust/commit/0953a2441a7741def008ac6df2be0072d3f67422))
- **coding-agent:** config selector TUI for pi config on a terminal ([1fc8936](https://github.com/yannelli/pi-rust/commit/1fc8936624146206939bd2eefd36a4597978a13f))
- **coding-agent:** port the 3D pi logo and Armin easter eggs ([feaefad](https://github.com/yannelli/pi-rust/commit/feaefad615a1fc1fe8a165e301ddddcb58ccc239))
- **coding-agent:** port the Earendil announcement with its bundled image ([51d10fa](https://github.com/yannelli/pi-rust/commit/51d10faa4e472e9fd1ae5a55d130b684313ca55a))
- **coding-agent:** port the /arminsayshi animation ([079acda](https://github.com/yannelli/pi-rust/commit/079acda7f02adc5ce8a147aa9d98f0ec1fb0e28a))
- **coding-agent:** port the session tree selector and branch summary prompt flow ([859ae47](https://github.com/yannelli/pi-rust/commit/859ae470025032f87f41357a00c46b9bce69097d))
- **coding-agent:** session selector with search, scopes, rename, and delete ([ceb1d5a](https://github.com/yannelli/pi-rust/commit/ceb1d5a3b3ac26e380283f0ee5d41d82a21e0e82))
- **tui:** built-in walker for @ file search without fd, and TUI hand-over ([de67dba](https://github.com/yannelli/pi-rust/commit/de67dba45d93034a6c890c965cd9836e51a00fe6))
- **coding-agent:** settings selector with theme, warnings, and per-model thinking submenus ([3181a44](https://github.com/yannelli/pi-rust/commit/3181a443856a9d2e8af85fd5c31e689f17973880))
- **coding-agent:** login dialog, provider and Radius selectors, model and scoped-model selectors ([6120c5c](https://github.com/yannelli/pi-rust/commit/6120c5ca7fd827d8dc3fb511a4df64bad9bbb8fa))
- **interactive:** chat components, tool renderers, and footer data ([ca4a7e7](https://github.com/yannelli/pi-rust/commit/ca4a7e7c43e486a2c1b1330e8e4276c9142fdf33))
- **coding-agent:** user message selector for /fork ([576b422](https://github.com/yannelli/pi-rust/commit/576b42228d3e5c015bd0149cec77b70b4864cd0e))
- **coding-agent:** port the first-time setup dialog and startup check ([a666b14](https://github.com/yannelli/pi-rust/commit/a666b144d0893a4636ce294fdfe1961d7e47a7b9))
- **coding-agent:** tool registry and built-in MCP, codemode, and tool_search in AgentSession ([79b8d0b](https://github.com/yannelli/pi-rust/commit/79b8d0b6483c3a982efe104f1c0d1a13feba766d))
- **js:** add fs, fs/promises, os, crypto, and readline ([3331312](https://github.com/yannelli/pi-rust/commit/33313120bb67b7947a94b79cf22bf5d2f50a1283))
- **coding-agent:** extension selector, input, and editor dialogs and the trust selector ([e321b41](https://github.com/yannelli/pi-rust/commit/e321b41083402b787d10561bee86bbb74484518a))
- **coding-agent:** capitalized key text and dialog module layout ([a8c45b0](https://github.com/yannelli/pi-rust/commit/a8c45b0bc822b996244d2a07a77afc98d81a2c4a))
- **coding-agent:** app keybindings, dialog building blocks, and simple selectors ([dd578c7](https://github.com/yannelli/pi-rust/commit/dd578c7e4bdc9f80006a939ccbc4828a29560fb5))
- **js:** add buffer, url, querystring, string_decoder, web globals, and streams ([e20ea62](https://github.com/yannelli/pi-rust/commit/e20ea62f6e64a2d07b3e651bfc20cdd87aa9349e))
- **interactive:** selector host for dialogs in the editor slot ([f75d7d5](https://github.com/yannelli/pi-rust/commit/f75d7d5624b2200bc5b6e95835da0ce0556c7db6))
- **coding-agent:** self-update from GitHub release archives ([8cedd15](https://github.com/yannelli/pi-rust/commit/8cedd153f8dc93db6e60b879664ac2298077155a))
- **tui:** port the Markdown component with a marked-compatible lexer ([89ce42e](https://github.com/yannelli/pi-rust/commit/89ce42e0a292876ff0f584e5070b4221f6fb1701))
- **coding-agent:** package commands, package resources, and shared tool factories ([77b09a7](https://github.com/yannelli/pi-rust/commit/77b09a7dc25a64b2399458fbe328b44c42c307f1))
- **js:** add pi-js crate with QuickJS runtime, TypeScript loading, and module resolution ([5f0fbbc](https://github.com/yannelli/pi-rust/commit/5f0fbbc15a224f8e7859ea58144bb837884d61ca))
- **mermaid:** render Mermaid diagrams as terminal art and add Markdown transformers ([f26990d](https://github.com/yannelli/pi-rust/commit/f26990d1661b98ea27f0937b19a3769bcb220407))
- **tools:** powershell tool, shell operations, tool factories, and tool metadata ([d780519](https://github.com/yannelli/pi-rust/commit/d78051937c6412367a46fc127ea0c89dd1237018))
- **agent:** pass the turn model to tools through ToolContext ([8ec6c64](https://github.com/yannelli/pi-rust/commit/8ec6c64c3d447aea492f2353d00cb302accd171a))
- **coding-agent:** bug reports, crash log, startup timings, and stdout guard ([de4b52d](https://github.com/yannelli/pi-rust/commit/de4b52d468dfaba48c345a39346a25da011867e9))
- **tui:** port the multi-line editor and autocomplete providers ([0165bd2](https://github.com/yannelli/pi-rust/commit/0165bd2a72ebd4468ed21a3dc969fdd86a32fb73))
- **theme:** themes, system theme, theme controller, and syntax highlighting ([03487c8](https://github.com/yannelli/pi-rust/commit/03487c8a064f6eb436ab850c39446a384fe298f6))
- **packages:** pi package manager, manifest, and package subcommands ([3ea1e22](https://github.com/yannelli/pi-rust/commit/3ea1e22cbf647e74697e243e046389fd6dae1e3f))
- **coding-agent:** llama.cpp router provider, remote model catalogs, and platform helpers ([ab666f1](https://github.com/yannelli/pi-rust/commit/ab666f11151a6e4443a0dda2b08a1172792aceff))
- **agent:** base stream options and upstream steering drain on continue ([e7e86bc](https://github.com/yannelli/pi-rust/commit/e7e86bc0b53bc88a2fc97c78929ab858f6e383f9))
- **agent:** remote stream proxy (streamProxy) ([c8a8118](https://github.com/yannelli/pi-rust/commit/c8a8118c4b6e8e1dc8f45698c0adfddca21c77d5))
- **tui:** port the LaTeX to Unicode renderer ([06e3d00](https://github.com/yannelli/pi-rust/commit/06e3d005a90edadb132d5a0e096f5d507f2925f2))
- **coding-agent:** agent session, runtime, and services for every mode ([83c5315](https://github.com/yannelli/pi-rust/commit/83c5315befb58ca6b0c8425bbd5e56e400db1e6d))
- **agent:** shared tool model, registry, nested tool calls, and validation parity ([d426b94](https://github.com/yannelli/pi-rust/commit/d426b949fbae3e2cd62cf46ea27156bf8d91ea03))
- **tui:** port Text, TruncatedText, Spacer, Box, Image, MouseRegion, SelectList, SettingsList, and loaders ([bbf50ba](https://github.com/yannelli/pi-rust/commit/bbf50bac0508237f46f915ddb0a63ce4e7053886))
- **agent:** declare tool loadout changes in transcript system messages ([c7c871d](https://github.com/yannelli/pi-rust/commit/c7c871d3ab73268b31c17a3055b4fac56c13ef8b))
- **agent:** finish_turn, prepare_request, and prepare_next_turn hooks ([08eb36e](https://github.com/yannelli/pi-rust/commit/08eb36e6d205818c8160ba6be1884c41ee58f63d))
- **mcp:** pi mcp add, remove, list, login, and logout as library functions ([f0e07d8](https://github.com/yannelli/pi-rust/commit/f0e07d8a0d572bc032fbdaca1e648bfd4f9eb506))
- **mcp:** tool adapters, resource tools, servers section, and session manager ([386311a](https://github.com/yannelli/pi-rust/commit/386311ac3e338858844cba843f9632a84e13b87d))
- **ai:** constrained sampling and Copilot headers for openai-completions ([b707987](https://github.com/yannelli/pi-rust/commit/b7079878356498703378704820df5f54c06a25fc))
- **ai:** openai-responses service tier option and tier pricing ([1f10f1e](https://github.com/yannelli/pi-rust/commit/1f10f1ed67f227173fd12765c89ea0c8fd992ef6))
- **ai:** constrained sampling, Copilot headers, and workload identity federation in core adapters ([81f6c1f](https://github.com/yannelli/pi-rust/commit/81f6c1fc6bfaf076bf01895b27fdf2eda310606a))
- **tui:** port Input component and add test component ports ([e1b4ca7](https://github.com/yannelli/pi-rust/commit/e1b4ca77c88ebaf795d556be0897c1f35ee3e82c))
- **mcp:** server connections, OAuth credential store, refresh, and sign-in ([236b5bc](https://github.com/yannelli/pi-rust/commit/236b5bcb739284fdb232ef3a3e0005a88e135816))
- **ai:** openai-completions parity with upstream ([76a53b4](https://github.com/yannelli/pi-rust/commit/76a53b494073a1a5752df4f95c9a7052aa87d04d))
- **coding-agent:** model selection, CLI flags, auth command, and auto retry ([7666ca6](https://github.com/yannelli/pi-rust/commit/7666ca6505830e125df04a1a6e6f332b35f4562f))
- **coding-agent:** settings manager with typed keys and upstream merge rules ([35580c9](https://github.com/yannelli/pi-rust/commit/35580c988c5267fda24719b2fc6a2ec76e602eb1))
- **ai:** auth.json credential store, auth resolver, env keys, and models.json extras ([8e319c7](https://github.com/yannelli/pi-rust/commit/8e319c7d9c8d08029d51bcef6e9b79f7d1899c42))
- **tui:** add pi-tui crate with renderer, terminal, keys, utils, colors, and images ([c3b6d0b](https://github.com/yannelli/pi-rust/commit/c3b6d0be818d599dba23398d4b9219b89a725f5f))
- **ai:** anthropic mid-conversation system messages, native tool changes, and managed effort ([a715cac](https://github.com/yannelli/pi-rust/commit/a715cac229b11ab7201b3f807fd46803461b0d41))
- **ai:** openai-codex-responses over SSE and cached WebSockets ([8e187ae](https://github.com/yannelli/pi-rust/commit/8e187ae9363711ad13632c437e537eb989bc304b))
- **ai:** classifier and image generation APIs ([5c014ec](https://github.com/yannelli/pi-rust/commit/5c014ec968a83a9d88de5f8a64eb76cd5c66c9fc))
- **ai:** azure-openai-responses provider and system message opt-in ([f5db614](https://github.com/yannelli/pi-rust/commit/f5db614f68417d4939b52614cbfc7c6fde2afef0))
- **ai:** JSON repair, surrogate escapes, and provider attribution headers ([ae64827](https://github.com/yannelli/pi-rust/commit/ae64827dc83a1981c115cc0e212e2ac920c9b54b))
- **mcp:** mcp.json config, value resolution, tool naming, result truncation, and server log ([4cdf620](https://github.com/yannelli/pi-rust/commit/4cdf620d78129653ddc9190971dfe63b2ad9b238))
- **ai:** pi-messages provider for the Radius gateway protocol ([ab1c6d2](https://github.com/yannelli/pi-rust/commit/ab1c6d24864dec1fc9c19528a146ecfc5398c624))
- **coding-agent:** clipboard file paths, pasted image files, and read tool model notes ([9bc2460](https://github.com/yannelli/pi-rust/commit/9bc24601d722c8098788e3afc5623073896be8d7))
- **ai:** openai-responses mid-conversation system messages and tool additions ([6349de1](https://github.com/yannelli/pi-rust/commit/6349de1dbf0c608296999a90b2e7b7d540b557d9))
- **session:** upstream session tree, entry types, and coding agent message roles ([7436482](https://github.com/yannelli/pi-rust/commit/7436482d15e555c4f4cb779bff32edbb5b000215))
- **ai:** port OAuth login flows and helpers ([57029c7](https://github.com/yannelli/pi-rust/commit/57029c7ceb8117252fe2cea527f68aebca06d442))
- **codemode:** codemode tool, host trait, models globals, and BM25 tool search ([851e910](https://github.com/yannelli/pi-rust/commit/851e910243e29d9554557a880699cb78d8603d27))
- **ai:** google-generative-ai, google-vertex, and mistral-conversations providers ([4014d2b](https://github.com/yannelli/pi-rust/commit/4014d2bfd2fd5c498aa7000b969b1af155579bfe))
- **coding-agent:** HTML and JSONL export, share, clipboard, and image resizing ([80b66eb](https://github.com/yannelli/pi-rust/commit/80b66eb4532aee847f91c135b0ab14b95a54e6d8))
- **ai:** Cloudflare Workers AI and AI Gateway URL templates and gateway auth ([0a223f4](https://github.com/yannelli/pi-rust/commit/0a223f4c750b041db0dbb583b019c4a59765978f))
- **ai:** bedrock-converse-stream with SigV4, event-stream framing, and the AWS credential chain ([d4e6de1](https://github.com/yannelli/pi-rust/commit/d4e6de1679d631ba7e471078c588fcb8f6cf579d))
- **ai:** github-copilot dynamic request headers ([5847428](https://github.com/yannelli/pi-rust/commit/5847428f2ef44b9e12a487f66130471cb46ce36d))
- **ai:** port constrained sampling with strict schemas and grammar tools ([f2ab040](https://github.com/yannelli/pi-rust/commit/f2ab040828190954c89aa7589b0e79a625cdeebc))
- **ai:** bundle every upstream provider and model type ([4af2415](https://github.com/yannelli/pi-rust/commit/4af241518f2ed2149eaa4ff8432569a8642b300b))
- **xtask:** generate the bundled model catalog from upstream data ([a806709](https://github.com/yannelli/pi-rust/commit/a806709ef7bec65c45b51b5abba51db6bf800f36))
- **mcp:** MCP client, stdio and streamable HTTP transports, and OAuth flow ([00b523b](https://github.com/yannelli/pi-rust/commit/00b523bac0d6ae15956e64a9d735c2d4b566ba73))
- **ai:** deferred responses, system message opt-in, and transcript tool resolution ([d5435bd](https://github.com/yannelli/pi-rust/commit/d5435bddec9f3872c5d7d860d47d83f30aa70606))
- **resources:** skills, prompt templates, project trust, and resource loader ([e3f2c47](https://github.com/yannelli/pi-rust/commit/e3f2c477389ac18f89da139e639b2b605cb59b3b))
- **codemode:** QuickJS sandbox, source options, and declarations ([1f862b8](https://github.com/yannelli/pi-rust/commit/1f862b82302d3534798e1a196993560f090a570e))
- **ai:** HTTP transport, provider retries, and request driver ([bd5c252](https://github.com/yannelli/pi-rust/commit/bd5c252a81ab66c82d7890ea1287f8b07236b04a))

### Fixed

- **extensions:** deliver detached events in the order they happen ([8c60ce3](https://github.com/yannelli/pi-rust/commit/8c60ce3250abbe588f01f1c96fa625857ebd13db))
- **ai:** treat a Windows lock directory pending deletion as busy ([002b59a](https://github.com/yannelli/pi-rust/commit/002b59a171a075d695cb135a5e7de55f83911f7f))
- **extensions:** settle response and stream hooks before passing stream events on ([f79ec65](https://github.com/yannelli/pi-rust/commit/f79ec659456945702319f41d420da1a639818355))
- **windows:** larger main stack, absolute module specifiers, and test paths ([a987df7](https://github.com/yannelli/pi-rust/commit/a987df7dfee408899d1cab396b8790381c998c74))
- **extensions:** deliver provider response and stream events in order ([190ccb0](https://github.com/yannelli/pi-rust/commit/190ccb02d5f1e66a771a9ebcb3d97e387e7b0d02))
- **acp:** treat rooted paths as absolute like Node and fix Windows and MCP test races ([86a0d4f](https://github.com/yannelli/pi-rust/commit/86a0d4fd7230d4e84ae7e7dc2e3ec1ac255845b6))
- **ci:** make path, key-hint, and flash-timing tests portable to Windows and macOS ([6a4a42f](https://github.com/yannelli/pi-rust/commit/6a4a42f6fc37ae62ac0ef1d2de8a3b1c1c38a076))
- **ci:** isolate session tests from the host home, keep LF checkouts, and optimize QuickJS in debug builds ([d776b12](https://github.com/yannelli/pi-rust/commit/d776b1254d6f3dca89fc092eeb493c3de148fad1))
- **ci:** gate unix-only test helpers, raise the codemode JS stack limit, and report compaction events on failure ([d5a5c29](https://github.com/yannelli/pi-rust/commit/d5a5c29dc33e3f22ebad02906d48301ebaf8d81c))
- **coding-agent:** show the server name in the interactive MCP sign-in prompt ([079c0bc](https://github.com/yannelli/pi-rust/commit/079c0bc798fbc5d5ec8da262cb3ce5eaef8d0642))
- **coding-agent:** request headers win over provider headers before attribution merge ([643c07b](https://github.com/yannelli/pi-rust/commit/643c07b040765f6b7081113080e19778bb3511b8))
- **rpc:** send only upstream session events to RPC clients ([39f76ce](https://github.com/yannelli/pi-rust/commit/39f76ced79dd9735c16df852324f1efd0316ce0d))
- **rpc:** detach built-in notices from sessions RPC mode stops serving ([f07fff2](https://github.com/yannelli/pi-rust/commit/f07fff28826a7a00261d6c9219e45a2428f916c6))
- **js:** build child_process and its tests on Windows ([385a17b](https://github.com/yannelli/pi-rust/commit/385a17b50fdfc352e132ef22c3ec90a1b3fae2f3))
- **tui:** guard yank-pop against a moved cursor ([ffc1164](https://github.com/yannelli/pi-rust/commit/ffc1164f311fcce9d577a28c2642a3149ad5f473))
- **mcp:** build the fake server and stdio tests on Windows ([8cfe82a](https://github.com/yannelli/pi-rust/commit/8cfe82af55a956b27c9496eb169eb8ea02ea1ef9))
- **compaction:** treat length stops that fill the window as overflow ([d79d6bd](https://github.com/yannelli/pi-rust/commit/d79d6bd46846e3d297f3ec74d8349aa5b50517b2))
- **codemode:** return without waiting for the VM teardown and stop workers of dropped runs ([950b8a6](https://github.com/yannelli/pi-rust/commit/950b8a679de57b8c53247ae7eb12d36ed11d2490))
- **ai:** match upstream login cancel message ([7b0ce5a](https://github.com/yannelli/pi-rust/commit/7b0ce5a75b155659e5babe9f54031013386f261a))

## [0.1.0] - 2026-10-12

### Added

- Rust port of the pi agent harness ([4170ecd](https://github.com/yannelli/pi-rust/commit/4170ecda8928b4a9e0b31b3a83d596dc1bee446f))
- scaffold pi-rust workspace with pi-ai contracts ([5a3bca1](https://github.com/yannelli/pi-rust/commit/5a3bca1cc259b4a6095ca24feeed67e4a18a6f92))
