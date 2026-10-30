# Changelog

All notable changes to pi-rust are documented here. Versions follow [Semantic Versioning](https://semver.org/); before 1.0, breaking changes bump the minor version.

## [0.8.0] - 2026-10-30

### Breaking Changes

- move the status line and iTerm2 integration to marketplace extensions ([62af9ce](https://github.com/yannelli/pi-rust/commit/62af9ce2e394e830764e207b157eb7dad132b3fe))

### Added

- **extensions:** extend native protocol v1 for SDK 1.1 ([3ef0614](https://github.com/yannelli/pi-rust/commit/3ef0614f966bd69d10ae7d84f333a8a0bd70ff85))
- **extensions:** select the extension runtime (quickjs, node, auto) ([2a314d2](https://github.com/yannelli/pi-rust/commit/2a314d2f51f81616d492aff777808be6fb32bdd0))
- **extensions:** send isProjectTrusted in the native ctx snapshot ([75ccd5c](https://github.com/yannelli/pi-rust/commit/75ccd5c779696fdb8f67eb1db1a20fdf978c4c4c))
- **js:** run extensions on a node child process ([f309c1b](https://github.com/yannelli/pi-rust/commit/f309c1b372e420afaefb33ea5dcb7f6cc9019d01))
- **tui:** browse and manage extensions ([742430d](https://github.com/yannelli/pi-rust/commit/742430ddeb969785b18ee5621aabbe78e7ae245c))
- **extensions:** expose SDK runtime modules and report export gaps ([6ee6d18](https://github.com/yannelli/pi-rust/commit/6ee6d183c0ea1e71da578a4c29ee4d8638361022))
- **extensions:** load native extensions through a JavaScript bridge ([79f71bb](https://github.com/yannelli/pi-rust/commit/79f71bb5e288cf80797fd114cfea0e0975678227))
- **extensions:** resolve pi.native entries as native: extension paths ([61b2474](https://github.com/yannelli/pi-rust/commit/61b2474fbdebeb9867d244011f720b8f1983b03d))
- **packages:** install signed marketplace extensions ([3fe599b](https://github.com/yannelli/pi-rust/commit/3fe599b5b665f1379b8f9de3e300d8228bdeab3e))
- **js:** add a backend selector to JsRuntimeOptions ([485c136](https://github.com/yannelli/pi-rust/commit/485c136743c46850295cb5ffef1f077e9d4fc014))
- **extensions:** add writeTerminal and footer stats, run state, and branch change hooks ([607a438](https://github.com/yannelli/pi-rust/commit/607a438aeb8cd8c217dddd6da75521978d62d5db))
- **tui:** configure providers and retry settings ([4bb5eae](https://github.com/yannelli/pi-rust/commit/4bb5eaecec884d49d5905689846028f27cb1d27d))

### Fixed

- **js:** make the Node backend and marketplace tests pass on Windows ([f54c65c](https://github.com/yannelli/pi-rust/commit/f54c65c264502905a97cdd4d4057490769fd51e9))
- **js:** drop an import that only the unix bootstrap test uses ([f3f2005](https://github.com/yannelli/pi-rust/commit/f3f2005f59edf49183eb173b0a7f03f3368f209c))
- **extensions:** close review gaps in native protocol 1.1 ([86ff4d5](https://github.com/yannelli/pi-rust/commit/86ff4d51a527aa1686ddfa9e1238478d3a8cb214))
- **extensions:** keep the session busy while a sendMessage-triggered turn starts ([baa0bb3](https://github.com/yannelli/pi-rust/commit/baa0bb36b4afbb0f7142b4a7960aa6e0636dc198))
- **js:** skip strings, comments, and regexes when hiding require calls ([7465168](https://github.com/yannelli/pi-rust/commit/7465168b5814136563bccbbc951436a0f972a250))
- **js:** move the Node backend protocol to a loopback socket ([8e44a9a](https://github.com/yannelli/pi-rust/commit/8e44a9a9239581d2c35d4ac51e22ebeb92888bcf))
- **extensions:** honor native event patches, gate ctx waits, report writeTerminal ([d8c3a0a](https://github.com/yannelli/pi-rust/commit/d8c3a0a1361c8a3a7d5adc54044fb6fa799995d7))
- **packages:** keep a marketplace install while another settings scope lists it ([2887954](https://github.com/yannelli/pi-rust/commit/2887954f77168eaf9d3a7311a3831e04e5f82ea4))
- **packages:** lock marketplace installs across processes with a lock file ([8a3069b](https://github.com/yannelli/pi-rust/commit/8a3069b1eeed85356529d10842c1c2e4dbc23dc1))
- **tui:** show pinned, up-to-date and offline marketplace updates in /extensions ([6525e78](https://github.com/yannelli/pi-rust/commit/6525e78ddf9ac5e0d12712c3061c40839af36364))
- **packages:** compare marketplace tags as semver and report pinned and offline updates ([8c79db0](https://github.com/yannelli/pi-rust/commit/8c79db00006f2405f38b46331e56ce4315f219d6))
- **packages:** skip and report an invalid marketplace entry so other packages still load ([c775e1b](https://github.com/yannelli/pi-rust/commit/c775e1b9b640007582cfd3f77298ca19f87c4581))
- **tui:** add a no limit choice for the server retry delay and fix the timeout description ([6a09112](https://github.com/yannelli/pi-rust/commit/6a09112a124a209982bcb88b8ac523f980a6c6c3))
- **js:** load registered CommonJS sources on Node before 22.15 ([e276c0c](https://github.com/yannelli/pi-rust/commit/e276c0c1c22fcd1f7e4baf9b8f6d3d2153d06320))
- **extensions:** hold native host requests until the first ctx ([b397eae](https://github.com/yannelli/pi-rust/commit/b397eae61e79242e056acb7afc8da3e1ef3f6287))

## [0.7.8] - 2026-10-30

### Added

- **ai:** add Cursor, Grok Build, and Google Antigravity providers ([630f541](https://github.com/yannelli/pi-rust/commit/630f541e84c3df90a36cf19394931617db509bb5))

## [0.7.7] - 2026-10-29

### Added

- **extensions:** add explicit CLI management commands ([50ab381](https://github.com/yannelli/pi-rust/commit/50ab381b0d9e73aafdfd3871c06be880d13c02bd))
- **iterm2:** offer confirmed current-pane setup with it2 ([314e6b5](https://github.com/yannelli/pi-rust/commit/314e6b58bcd0e4c8ea9ac84f40e6bb2df3c902f2))
- **models:** support global provider exclusions ([2b596ff](https://github.com/yannelli/pi-rust/commit/2b596ff2b03c01ac9f26cdb35a7172c383df4d95))

### Fixed

- **ai:** use the bound ChatGPT OAuth callback port ([ebdbaed](https://github.com/yannelli/pi-rust/commit/ebdbaed17567cc582d92b71cfec4f8c5c80d2ea3))
- **statusline:** terminate command input and preserve output on failure ([91f7acf](https://github.com/yannelli/pi-rust/commit/91f7acfedb2a0a91316d0a2b0f5432f175ca23b2))
- **tui:** recall slash commands and clear drafts with Escape ([5eab54a](https://github.com/yannelli/pi-rust/commit/5eab54a5ba8393fb4db84cd7cfa32c88d0549527))
- **acp:** separate permission modes from thinking controls ([c616f1c](https://github.com/yannelli/pi-rust/commit/c616f1c23caac15775b3fa48dca23818f7a84649))

## [0.7.6] - 2026-10-29

### Added

- **tui:** add statusline editing and matching themes (#120) ([19bc346](https://github.com/yannelli/pi-rust/commit/19bc3461f027b2720f0475b90a5b48ea3887ba90))

## [0.7.5] - 2026-10-27

### Added

- **coding-agent:** add an optional iTerm2 integration and /terminal-setup ([a152176](https://github.com/yannelli/pi-rust/commit/a1521768b42d5d2bf097beb2ab77758954bc4ae1))
- **coding-agent:** add a customizable status line ([e36f66f](https://github.com/yannelli/pi-rust/commit/e36f66f44f182c9c55f68205ab62aac78ea4c026))

### Fixed

- **coding-agent:** draw the original pi logo in rust colors ([cc1ba11](https://github.com/yannelli/pi-rust/commit/cc1ba112157b3265dd11564879e97e347926480f))

## [0.7.4] - 2026-10-27

### Fixed

- **coding-agent:** draw the pi-rust gear logo in the TUI ([24a2fbe](https://github.com/yannelli/pi-rust/commit/24a2fbeb00e40d85bdf868982221ee62240426dc))

## [0.7.3] - 2026-10-27

### Fixed

- **release:** ship the licenses of code ported from Node.js and the MCP TypeScript SDK ([5afaf56](https://github.com/yannelli/pi-rust/commit/5afaf565b471dfcc895778fe73194549721193b1))

## [0.7.2] - 2026-10-27

### Fixed

- **release:** ship the licenses of third-party crates in the release archives ([28b9bc4](https://github.com/yannelli/pi-rust/commit/28b9bc40089fa4b358cb1fe6ec8a6d6517589646))

## [0.7.1] - 2026-10-27

Maintenance release.

## [0.7.0] - 2026-10-27

### Breaking Changes

- **durable,chord:** publish commits at adoption, address-following watches, synchronous observers (upstream 8b0caefc2) ([b4b9f18](https://github.com/yannelli/pi-rust/commit/b4b9f188ea1785ece86f57a0651f7c71e879e998))
- **ai,agent:** parse streamed tool-call arguments incrementally (upstream f715a15c1) ([cc04d32](https://github.com/yannelli/pi-rust/commit/cc04d329bcb0cd9147c6da604a4183c82baa6a46))
- **chord,durable:** Storage adopts inside the commit, watch types in Chord, explicit watch retirement (upstream 92ab53602) ([3dd52d4](https://github.com/yannelli/pi-rust/commit/3dd52d4cee8dfdb7105935cafca5af93694a4ff9))

### Added

- **durable:** partial commits append instead of assigning (upstream da412b039, durable part) ([6447346](https://github.com/yannelli/pi-rust/commit/6447346197cc795267dbdbeb919b8a2aa91bf631))
- **coding-agent:** let inline ctx.ui.custom() components report blocked program status (upstream c1810bf70) ([e8c378d](https://github.com/yannelli/pi-rust/commit/e8c378d42cf508d804059ec6839843e765c702e2))
- **durable:** deferred storage of throttled commits (upstream 6c2fd73b0) ([d117f73](https://github.com/yannelli/pi-rust/commit/d117f7302406cffedd3e092ff104e7effdfc7b83))
- **coding-agent:** show model names in model selectors when they add information (upstream 606af8662) ([19c8b2e](https://github.com/yannelli/pi-rust/commit/19c8b2e20d43d19440a50563d04b98f54ed4371c))
- **coding-agent:** confirm device-code browser and clipboard actions (upstream 7151099d0) ([edd0284](https://github.com/yannelli/pi-rust/commit/edd0284a41d344be33a33855e8d4e5975be3b23b))
- **coding-agent:** add messageId and timestamp to markdown transformer context (upstream 64627838b) ([284d8f6](https://github.com/yannelli/pi-rust/commit/284d8f647744bcedb589bc9c1d51417d346b4c25))
- **chord,durable:** PendingOps folds pending ops for slow consumers; Watch delivers one call per busy period (upstream d8d99950a) ([2a7c2a2](https://github.com/yannelli/pi-rust/commit/2a7c2a2ebfab4786f2aa644e50722508ff47ca06))
- **chord:** append_text records a string append without comparing the strings (chord part of da412b039) ([29aab97](https://github.com/yannelli/pi-rust/commit/29aab973ff1d297a1d0d57af4ca9a36dc3948ccf))
- **durable:** open a Harness read-only (upstream be14d9732) ([881a73b](https://github.com/yannelli/pi-rust/commit/881a73b46ddc5b6dbe45b8f4ccf12c4a10a9f79a))
- **ai:** regenerate models.json for Kimi K3.1 Pro and GPT-6.1 Terra (upstream 1de2d7ade, upstream 363ad78bf) ([cc91ffb](https://github.com/yannelli/pi-rust/commit/cc91ffb5b8ebf0be17ef18088b86865deb0ced62))
- **chord:** NativeWatchable, the in-process counterpart of Watchable (upstream 5e106ab1c) ([cbc6557](https://github.com/yannelli/pi-rust/commit/cbc6557b28b5a20a2ec21d8b2267153f7702384d))
- **chord:** shared watch-handle implementation (Watch, NativeWatch); Durable uses it (upstream 42cb822f1) ([2b310d0](https://github.com/yannelli/pi-rust/commit/2b310d0b7f6198e2a6c63bcf13b58bef8121e955))
- **ai:** image input for System One classifiers (upstream 7f225f25d) ([bc9f6f7](https://github.com/yannelli/pi-rust/commit/bc9f6f75ce2716582371589000cf6298d3aabe10))
- **ai:** add incremental StreamingJsonParser and ToolCallArgumentsStream (upstream f715a15c1) ([db269b2](https://github.com/yannelli/pi-rust/commit/db269b2147880ac8d5cde1f7a2e4660abf17bbac))
- **chord:** Watchable, the common interface of values that can be watched (upstream a2fd25f56) ([8b104a8](https://github.com/yannelli/pi-rust/commit/8b104a85adc625fdd78c1a6750a01a8e468403a9))
- **ai:** add totalFromProvider flag to usage cost (upstream 7c22a93d0) ([21a7780](https://github.com/yannelli/pi-rust/commit/21a7780cda83958c1a9683654e1e9db6d237349f))
- **ai:** carry responseModel on pi-messages terminal events (upstream 14c811841) ([bd8ec76](https://github.com/yannelli/pi-rust/commit/bd8ec761bd59636f59eff6236f4288b15baa9acc))

### Fixed

- **durable:** take a partial's changes when its commit is scheduled ([ba36d68](https://github.com/yannelli/pi-rust/commit/ba36d68b6287cf41f2b95ac91e1a9a84d3e36742))
- **coding-agent:** hidden thinking with an empty label renders no rows (upstream a6a7bac86) ([3be4239](https://github.com/yannelli/pi-rust/commit/3be42390087055f6a9ebb88e2fe08d7f7effb7e4))
- **coding-agent:** skip npm audit and fund output when installing pi packages (upstream 5fb9a0b11) ([1e17af4](https://github.com/yannelli/pi-rust/commit/1e17af4d8055c1d0a3e68b4cbfbf345566c4267f))
- **env:** watch a directory again once the backend dropped its watch (upstream issue 11342) ([05b4a5b](https://github.com/yannelli/pi-rust/commit/05b4a5b283c0a8136ae87a96e59490965896e88e))
- **tui:** recognize kitty F1-F12 sequences and ignore F13+ keys (upstream deb12d78e) ([2dd5179](https://github.com/yannelli/pi-rust/commit/2dd51796c18801cbc4950ceab5a548660bea41a1))
- **coding-agent:** default openai provider to gpt-6.1-sol (upstream f049787d9) ([2e7cda6](https://github.com/yannelli/pi-rust/commit/2e7cda6e19cd8b26683615a9a533df4e5de3b3f5))
- **coding-agent:** keep open extension dialog visible when the editor component is replaced (upstream 7853186d1) ([07caa50](https://github.com/yannelli/pi-rust/commit/07caa5093fa7b8707c53d083f689b66f8356febe))
- **coding-agent:** keep grep context lines aligned when a file has a lone CR (upstream fecc19e4e) ([7abe518](https://github.com/yannelli/pi-rust/commit/7abe51879fc3d09c2b507675defae3b39795ac5a))
- **durable:** record the requested thinking level on assistant messages (upstream d5b3de22d) ([f001178](https://github.com/yannelli/pi-rust/commit/f0011781a867f953148e1004bb0ab49c45618747))
- **durable:** continue runs after tool handoffs (upstream 38ff99177) ([40c3300](https://github.com/yannelli/pi-rust/commit/40c330092fe5862b8f874b44e467bdcf6261492a))
- **coding-agent:** cap pi.exec output and stop the command at the cap (upstream issue 11360) ([4cb1c6c](https://github.com/yannelli/pi-rust/commit/4cb1c6c1196a5d098a4c9dcf1fb80bda0915633b))
- **coding-agent:** refresh unpinned -e npm packages when a newer version exists (upstream issue 11375) ([e8171ff](https://github.com/yannelli/pi-rust/commit/e8171ffd09fc15331147bd505ca40b002300479f))
- **coding-agent:** wait for extension prompts before print mode exits (upstream issue 11238) ([998059c](https://github.com/yannelli/pi-rust/commit/998059ca78c2cceacaa5e79e704eba02dcf3a35a))
- **coding-agent:** strip CSI sequences with private parameter bytes (upstream issue 11377) ([9188c56](https://github.com/yannelli/pi-rust/commit/9188c5688a0bbd646ed5253f9afc4a0e20f50ebf))
- **coding-agent:** record the live thinking level after /tree navigation (upstream issue 11451) ([0443e6a](https://github.com/yannelli/pi-rust/commit/0443e6a7ca1e4a0b52deadcfeaca87ba3964cb58))
- **coding-agent:** pass the selected thinking level to tree branch summaries (upstream issue 11444) ([7a4d18f](https://github.com/yannelli/pi-rust/commit/7a4d18f2f131fb5420466c57e0396df1d6b3f4f4))
- **coding-agent:** name settings.json in the package install warning (upstream 6dfb54678) ([fb985d2](https://github.com/yannelli/pi-rust/commit/fb985d2fdb1e16c0b1daa01c0243445c6dd6dea8))
- **examples:** todo renderResult returns undefined on validation errors (upstream 3953e9ef4) ([1bb4686](https://github.com/yannelli/pi-rust/commit/1bb4686d23afa4cf97d41780e27db3e0d048e919))
- **coding-agent:** render streamed tool-call arguments once per frame, upstream f715a15c1 (tool rendering part) ([469de92](https://github.com/yannelli/pi-rust/commit/469de927d4d659f2a318c0c95af7e7bd3a1dd442))
- **coding-agent:** rank tools first when the search query is their name (upstream ae823bc0b) ([13e4735](https://github.com/yannelli/pi-rust/commit/13e47354efdcabdee5d1ae206afce04eeee4c870))
- **coding-agent:** resolve provider/id references when the provider id contains a slash (upstream 984cf6643) ([3ee9265](https://github.com/yannelli/pi-rust/commit/3ee9265ada12f7a4bb9631380716a8a8a9170d73))
- **coding-agent:** pass real path as npm prefix when agent dir is a symlink (upstream 5f960a0e2) ([7753279](https://github.com/yannelli/pi-rust/commit/77532792a86b6966a13f8b268b98bb40e44f03c0))
- **tui,coding-agent:** rank contiguous fuzzy matches and search session fields separately (upstream 8c03fef4d) ([5c1ef45](https://github.com/yannelli/pi-rust/commit/5c1ef455960eed2aa1465637681ce8cf5c342ed6))
- **coding-agent:** login dialog opens only web URLs, upstream 89c921d8b (login dialog part) ([82243d9](https://github.com/yannelli/pi-rust/commit/82243d9cfe6f67c98735af8cbaa47a3eb31412fd))
- **coding-agent:** decode pi.exec output across UTF-8 chunk boundaries (upstream 36be9c612) ([1281ecd](https://github.com/yannelli/pi-rust/commit/1281ecd793d32302d495af37aa26bf31fe0a84cb))
- **coding-agent:** render HTML export for sessions deeper than the call stack (upstream 1c720d285) ([97cb781](https://github.com/yannelli/pi-rust/commit/97cb781a7b89b94b0426b96f473b96d646e41b99))
- **coding-agent:** render system messages in HTML export (upstream 9359adf0c) ([6a48240](https://github.com/yannelli/pi-rust/commit/6a48240516e917425b1558727d76c453778ffce9))
- **coding-agent:** stop macOS shell groups before killing them (upstream 27cc6bfc0) ([6654a49](https://github.com/yannelli/pi-rust/commit/6654a4979b30699c6d5b5504cf4773b482f0e731))
- **ai:** apply modelOverrides to classifier and image models (upstream issue 11223) ([2a86741](https://github.com/yannelli/pi-rust/commit/2a86741aac7d0519abc963dbd4c8a580331881bb))
- **ai:** keep Bedrock strict tool sets within the union parameter limit (upstream issue 11416) ([c181a17](https://github.com/yannelli/pi-rust/commit/c181a17fb290b11c7d88a7245ee862a4329b315b))
- **ai:** do not send tool_removal for top-level Anthropic tools (upstream issue 11376) ([0de093e](https://github.com/yannelli/pi-rust/commit/0de093e95cbd8f3399f538a225c283e030595827))
- **ai:** harden OAuth URLs, pi-messages content indexes, and the CLI auth file (upstream 89c921d8b (ai part)) ([ce06cbd](https://github.com/yannelli/pi-rust/commit/ce06cbde8d54e15287230e93a92bedaad9544f80))
- **tui:** strip pasted escape sequences and end unfinished pastes (upstream issue 11383) ([cbbb8ae](https://github.com/yannelli/pi-rust/commit/cbbb8ae61c60eb35260b7a4212cac652f2848f62))
- **tui:** match F1 to F12 in Kitty forms and with modifiers (upstream issue 11345) ([6cc491f](https://github.com/yannelli/pi-rust/commit/6cc491fe15272adb4ba50c392a687fb5d09b2388))
- **mcp:** apply maxMessageBytes to HTTP JSON responses (upstream issue 11335) ([eaaa4dd](https://github.com/yannelli/pi-rust/commit/eaaa4dd4eb11268245951ab121171c9bb8d8e003))
- **codemode:** let scripts declare top-level tools and console (upstream issue 11226) ([555cb0f](https://github.com/yannelli/pi-rust/commit/555cb0f846b0639202902780041ec24f09c777e5))
- **ai:** use OpenRouter-reported total cost (upstream cae8d69e5) ([8cdbd99](https://github.com/yannelli/pi-rust/commit/8cdbd99f3c1f09ef3288fefd185b9fea4adb9ce9))
- **ai:** do not count faux cache writes as input tokens (upstream f3fa8bd92) ([44596ff](https://github.com/yannelli/pi-rust/commit/44596ffe0027404e0d9c589d467d5dc26c610458))
- **tui:** replace control characters in terminal titles (upstream 89c921d8b (tui part)) ([13a4c26](https://github.com/yannelli/pi-rust/commit/13a4c26eb8ed22632546f677d61fdff514f47650))
- **tui:** bound image line search by tallest registered image (upstream 1c8cb1781) ([8f87866](https://github.com/yannelli/pi-rust/commit/8f87866560852578251f38687f41aef317fa9efe))
- **tui:** crop boxed images scrolled above the fullscreen viewport (upstream 4464442f9) ([7f86cf2](https://github.com/yannelli/pi-rust/commit/7f86cf2298283c01a44ee6f44be7375cd7dd1961))
- **ai:** send fixed low top-level effort with Anthropic per-message effort (upstream e3bc05e87) ([95ee17b](https://github.com/yannelli/pi-rust/commit/95ee17b4c0ef415ebf26580ccfbd8853343c55da))
- **tui:** use iterm2 word characters for fullscreen double-click selection (upstream ff6896471) ([8aecadc](https://github.com/yannelli/pi-rust/commit/8aecadcd5bb8e8df742b9180073f4c6d04fc53ed))
- **tui:** stop nested blockquote rendering from growing exponentially (upstream 8a2ebd385) ([276e843](https://github.com/yannelli/pi-rust/commit/276e8439d8a5a5386e5ea1a40598ff33376c5f3f))
- **ai:** preserve literal data in Gemini tool schemas (upstream a7b94a03f) ([e6e8ed5](https://github.com/yannelli/pi-rust/commit/e6e8ed5f530e5fd7dbd32cc4fa6e2176d76a685b))
- **mcp:** accept only http and https OAuth URLs (upstream 9ffd27cfa) ([66e1b65](https://github.com/yannelli/pi-rust/commit/66e1b659a54dc887cd7121bcca89b378cd662566))
- **ai:** send const as enum in Gemini tool schemas (upstream 7314819f0) ([e7e3099](https://github.com/yannelli/pi-rust/commit/e7e3099ccd0ad8f2985cb2ea95d02fce5ddef2fa))

## [0.6.3] - 2026-10-27

### Added

- **coding-agent:** add the pi-rust logo ([7d2a345](https://github.com/yannelli/pi-rust/commit/7d2a34559b5a2c0efcec9a3a302c78d868d7c6f4))

## [0.6.2] - 2026-10-21

### Added

- **interactive:** add /retry to run the last prompt again ([a0f9484](https://github.com/yannelli/pi-rust/commit/a0f948483087058d6d83221768f37892dab1bc43))
- **interactive:** add /review for uncommitted changes ([87220e8](https://github.com/yannelli/pi-rust/commit/87220e84855d0d33f2fc2c8d9bb01f9231a24cd2))

## [0.6.1] - 2026-10-21

### Added

- **coding-agent:** add /context to show context window usage ([da84781](https://github.com/yannelli/pi-rust/commit/da84781e659f51720ab0431a81b874e787ca1828))
- **interactive:** add /undo for agent file changes ([ca18774](https://github.com/yannelli/pi-rust/commit/ca18774bb1b2cb6a17a0261ddc025fdac009d70a))
- **interactive:** add /diff to show uncommitted git changes ([2800be9](https://github.com/yannelli/pi-rust/commit/2800be9b075ec3f1ea23517e6bc541ee50aefa1c))
- **interactive:** add /init to write or improve AGENTS.md ([0bed88c](https://github.com/yannelli/pi-rust/commit/0bed88c990cb80573c34267b7cd0e8172f75e68c))
- **coding-agent:** --output-schema for print mode ([b62152a](https://github.com/yannelli/pi-rust/commit/b62152a5579a43b476a1c5e633f2f3abde013389))
- **ai,agent,coding-agent,durable:** move client identity and provider attribution into pi-ai (upstream adc5fa9f8, upstream 04a93bad8) ([f6198da](https://github.com/yannelli/pi-rust/commit/f6198da459e394ed52f18e5c59bfaf54e348f73e))
- **chord:** chord2 reload, code version retirement and clusters ([f4e9476](https://github.com/yannelli/pi-rust/commit/f4e9476202c91557a78a0e924864be1aa9a0ee3c))
- **chord:** chord2 connections, remote refs and WebSocket links ([c238924](https://github.com/yannelli/pi-rust/commit/c238924c9673c9a3943f196437872f1306a139f6))
- **coding-agent:** pi stats ([a6e9262](https://github.com/yannelli/pi-rust/commit/a6e926209df5ea1160fb289bab3723962c3abea6))
- **coding-agent:** --max-turns and --max-cost run limits ([a391aac](https://github.com/yannelli/pi-rust/commit/a391aacde24e70659aa3b2f479f099f12528e381))
- **coding-agent:** pi sessions list, rm and prune ([fd08285](https://github.com/yannelli/pi-rust/commit/fd08285033549cab549a4c32f4358aea643d5a3f))
- **coding-agent:** export sessions as Markdown ([7907fed](https://github.com/yannelli/pi-rust/commit/7907fed8d4a2046bdaa019e518e61b65beada558))
- **chord:** chord2 in-host runtime ([0f579c5](https://github.com/yannelli/pi-rust/commit/0f579c516316ff54f6de45a7637167e1480aaa07))
- **chord:** chord2 cells and objects ([afcf341](https://github.com/yannelli/pi-rust/commit/afcf3414ff4bbf4004ee7e80f812329652c49f07))
- **tui:** persistent prompt history with Ctrl+R search ([c2ac3e6](https://github.com/yannelli/pi-rust/commit/c2ac3e6b5e51649b89d4251dd229ac5b762626e2))
- **coding-agent:** pi doctor ([f12d809](https://github.com/yannelli/pi-rust/commit/f12d809816908c0f6fbd612ee9f77ccfb1d7f5c8))
- **coding-agent:** pi completion for bash, zsh, fish and PowerShell ([a0958ee](https://github.com/yannelli/pi-rust/commit/a0958ee928925a47870dccb2445d7cd982fdd765))
- **coding-agent:** notify when a long run finishes ([b4b0efe](https://github.com/yannelli/pi-rust/commit/b4b0efef528ed985e696382e27304f06c7bb6d41))
- install scripts for Linux, macOS and Windows ([7052bcd](https://github.com/yannelli/pi-rust/commit/7052bcdb66595712b028c8b91d54b269532d9615))
- **chord:** chord2 host manager and IPC links ([6add286](https://github.com/yannelli/pi-rust/commit/6add2863f116482f126a284c7ebdff613f365a3e))
- **chord:** chord2 wire frames, payload packing and memory links ([b2650b7](https://github.com/yannelli/pi-rust/commit/b2650b788a98069c23e3d37c07efc5f5b0793a3b))
- **chord:** chord2 errors, schema and contracts ([a5f8b5b](https://github.com/yannelli/pi-rust/commit/a5f8b5b61f5834747070a9184a60a97191361d9f))
- **schema:** pi-schema crate with the shared TypeBox-compatible validator, Value.Default and Value.Clean ([1e03274](https://github.com/yannelli/pi-rust/commit/1e03274d8ae2e8e2ce40692cccf0a60c42ff61c3))
- **durable:** SQLite TelemetryStore (upstream d9025a5f5, telemetry-sqlite half) ([fb13de1](https://github.com/yannelli/pi-rust/commit/fb13de1d44658951e90b3f917292a432a4ce4aaa))

### Fixed

- **ai:** sync the model catalog with upstream model data ([de80f34](https://github.com/yannelli/pi-rust/commit/de80f347589c975f8e4e66847cb4b3ae63777b13))
- **coding-agent:** skip a configured package that fails to install ([f9b4f07](https://github.com/yannelli/pi-rust/commit/f9b4f072e2c0c8c37120785f9414f1952ff30b8d))
- **ai:** lead with the first system message after session start context ([3ef1a65](https://github.com/yannelli/pi-rust/commit/3ef1a658f0ad4015427c745b9f557948aa6417bd))
- **coding-agent:** start a triggered extension message after compaction ([32cc54c](https://github.com/yannelli/pi-rust/commit/32cc54c89e83589da9a0bc500c906c9e5a02aa71))
- **durable:** give the built-in read tool a structured output schema ([130304f](https://github.com/yannelli/pi-rust/commit/130304f791e1b667b2f21c6c2210641a41feb8ac))
- **coding-agent:** rename the live session from the resume picker ([aa3cf0d](https://github.com/yannelli/pi-rust/commit/aa3cf0d43e8a9a0a843037976827e8d860ffaa6c))
- **tui:** keep the leading ~ when double-clicking a home path ([d01ca03](https://github.com/yannelli/pi-rust/commit/d01ca03ef830669d751bb238541aa5ae24ddf2b4))
- **tui:** rejoin a mouse report that the input timeout split ([98bb310](https://github.com/yannelli/pi-rust/commit/98bb310ca3d588203486c6f7bdf5ed8d10d3b7a7))
- **models:** refresh extension providers offline after a virtual model is removed ([69ba4fc](https://github.com/yannelli/pi-rust/commit/69ba4fc797c466bbf4eddcd1360038331e2581ca))
- **models:** refresh extension providers offline after registration ([da1a095](https://github.com/yannelli/pi-rust/commit/da1a0956a804747c0360ff37ff0029c4c3c42c6a))
- **codemode:** drop the bare open-object term next to typed allOf parts ([12f4d03](https://github.com/yannelli/pi-rust/commit/12f4d039189da20ddc19c8409a8735605ec9f8b3))
- **codemode:** keep deactivated direct tools hidden in only mode ([db0df5e](https://github.com/yannelli/pi-rust/commit/db0df5e23ba03bc607b593724691702a67438f8b))
- **codemode:** keep the sibling type of allOf constraints in declarations ([b4419e9](https://github.com/yannelli/pi-rust/commit/b4419e9c572686b2da8ce10785e3ba482d667ecf))
- **coding-agent:** report why pi.exec could not start a command ([2f6b4b2](https://github.com/yannelli/pi-rust/commit/2f6b4b2fb94ba33d2452375e548b29ac51941f02))
- **ai:** take subscription login context sizes from the OpenAI model list ([98c4720](https://github.com/yannelli/pi-rust/commit/98c4720c2eefff3edc5a4ddc193799ce6ccb19f8))
- **ai:** let uncached Codex retries fall back to SSE ([71a8760](https://github.com/yannelli/pi-rust/commit/71a876086b850e3de7dee2fc75252ed72d33e25e))
- **coding-agent:** omit tool results with the response in pre-prompt recovery ([3a3a378](https://github.com/yannelli/pi-rust/commit/3a3a37877deb344e2044b30d49f526511940f238))
- **ai:** hold Anthropic system updates past assistants with no blocks ([27fd19f](https://github.com/yannelli/pi-rust/commit/27fd19f33389e369d973e8b2358a20e34f71d002))
- **js:** port node:readline append-only input for TERM=dumb ([82a935a](https://github.com/yannelli/pi-rust/commit/82a935af3cbf75acdcbecddc39b83991f4a15a0f))
- **durable:** remove the exclusive SQLite operation and the replicated connection (upstream 4759f09fb, upstream 89e309581) ([eb5fac3](https://github.com/yannelli/pi-rust/commit/eb5fac30806a2c2dbf283e6bf199f090fd5734fa))
- **coding-agent:** activate extension tools registered after the branch recorded its loadout (upstream bd3c9b823) ([7c2c87d](https://github.com/yannelli/pi-rust/commit/7c2c87dc4653edaf8ff7c7cb9a9a7be88694c083))
- **coding-agent:** apply /compact instructions to the split-turn prefix summary (upstream adfc14461) ([d3a443e](https://github.com/yannelli/pi-rust/commit/d3a443ed402da8c8182d7480c9be96d096103d95))
- **tui:** escape regex metacharacters in single-segment fd queries (upstream 612a172e6) ([f15fb70](https://github.com/yannelli/pi-rust/commit/f15fb70b7c76c676b74d02692ead16535ce6668e))
- **ai:** retry relayed stream disconnects and match status codes as standalone numbers ([55a7fc8](https://github.com/yannelli/pi-rust/commit/55a7fc8accaed7423bf664542cacc7d1c7ea8d53))
- sync Cargo.lock with the v0.6.0 workspace version ([eb35852](https://github.com/yannelli/pi-rust/commit/eb358527684b120a2e0c09ddc93d4b1cec14f0f7))
- sync Cargo.lock with the v0.6.0 workspace version ([1cfa94e](https://github.com/yannelli/pi-rust/commit/1cfa94ee11df72205e5539af2268322b8db90227))
- **experimental:** complete a Session release when its worker retires ([bfb5f45](https://github.com/yannelli/pi-rust/commit/bfb5f45d5a195a2f37af7c18573bb5e0b6f59231))
- sync Cargo.lock with the v0.6.0 workspace version ([9de5692](https://github.com/yannelli/pi-rust/commit/9de5692e5d55a7bf702ce587ea92aa86fe5772c8))
- sync Cargo.lock with the v0.6.0 workspace version ([c49590f](https://github.com/yannelli/pi-rust/commit/c49590f90825c6755747866531a1e9a524e2c7e3))
- sync Cargo.lock with the v0.6.0 workspace version ([95ee714](https://github.com/yannelli/pi-rust/commit/95ee714397322df2070b2f9f755db1f9a86af563))
- **js:** drain a closing Unix socket like TCP ([c5aec1c](https://github.com/yannelli/pi-rust/commit/c5aec1cf7cdcb706bb3f2a2ceda80ff395c5edd6))
- sync Cargo.lock with the v0.6.0 workspace version ([b8e05af](https://github.com/yannelli/pi-rust/commit/b8e05af2d1ca4eb96cd8d9d30b14290bf95305ce))
- **js:** report an aborted socket read on Windows as ECONNRESET ([49336fa](https://github.com/yannelli/pi-rust/commit/49336faad2ef777a6d2dd4a99b68def43ff155ee))
- **js:** keep a closed HTTP/2 session's socket open until the peer ends it ([04623dc](https://github.com/yannelli/pi-rust/commit/04623dca65d88b1c0fd41484fa92fb40ec002e0e))
- **js:** keep an HTTP/2 session open after its own GOAWAY ([5dfacd8](https://github.com/yannelli/pi-rust/commit/5dfacd8232db377716532d353e65cfeb9d8405ea))
- **js:** let promise jobs run between HTTP/2 SETTINGS frames ([d9e18ca](https://github.com/yannelli/pi-rust/commit/d9e18ca0e38db87bad6abf0738344b90f007c157))
- **js:** emit tlsClientError before the TLS client sees the close ([6fffd9f](https://github.com/yannelli/pi-rust/commit/6fffd9f9fccb7c49d079ef537d086cd65f1dfce0))
- **coding-agent:** accept Windows Store shell aliases (upstream 28bea006b) ([df93a03](https://github.com/yannelli/pi-rust/commit/df93a031743e2e9dffc0e0a80e3ee8e8f867e412))

## [0.6.0] - 2026-10-21

### Breaking Changes

- **durable:** reject waits that close a wait cycle (upstream eb2fce43d) ([8a300b4](https://github.com/yannelli/pi-rust/commit/8a300b4d227875480efce797986f9440ee897c33))
- **durable:** deferred tools and the tool_search extension (upstream 77332f7a3) ([3b918c9](https://github.com/yannelli/pi-rust/commit/3b918c9ebae7387a03470804682232d242fdd138))
- **durable:** inputs placed as application entries, display-only writes appended at once (upstream e6b9a6af5) ([5177658](https://github.com/yannelli/pi-rust/commit/5177658b5c18998dbdca2460b15e730c25c05ed7))
- **durable:** one document per nested call; TaskRuntime::owned_tasks() (upstream 4ac0bd8c7) ([ad16fa4](https://github.com/yannelli/pi-rust/commit/ad16fa44054955c673b4354071eabbf653146cc4))
- **telemetry:** add TelemetryStore, trace renderer, and trace IDs (upstream d9025a5f5) ([8f9980c](https://github.com/yannelli/pi-rust/commit/8f9980c8570ef8d5a2decdd13908cc6af1203a56))
- **durable:** require checkpoint_when on document definitions (upstream 5c1736f82) ([a5d0ab4](https://github.com/yannelli/pi-rust/commit/a5d0ab42d7c475101f7e9c2585821fc3734046f6))

### Added

- **durable:** optional FileSystem::list_files for tree listings (upstream abfe62928) ([9d2a8f9](https://github.com/yannelli/pi-rust/commit/9d2a8f95febdea7832eda40461fe1513705ebeb0))
- **durable:** code mode listing, tool search, output, models.*, grammar; tool error rule (upstream 8bb180bec) ([0065bda](https://github.com/yannelli/pi-rust/commit/0065bda213773c23eb84a32c15fecd7635343bd9))
- **coding-agent:** add PI_CLIPBOARD_OSC52 to force OSC 52 copies (upstream 3e477eab5) ([6794362](https://github.com/yannelli/pi-rust/commit/679436236f089e5abd59e0ea4ca2c4fbc5c387e2))
- **coding-agent:** show elapsed and total time on codemode rows (upstream 014873d7d) ([63062b3](https://github.com/yannelli/pi-rust/commit/63062b3900ceb3b1aacb092fa39d75bdf8c73567))
- **durable:** bound the Session's document cache by use (documentRetentionMs) (upstream f44707e7f) ([d10b7af](https://github.com/yannelli/pi-rust/commit/d10b7affd73cce6ec4750321cb12bff9eab41fff))
- **durable:** code mode result lists the script's tool calls (upstream 127b77121) ([2eee4de](https://github.com/yannelli/pi-rust/commit/2eee4de5eb342711ebd90d404d43ded9a19aae51))
- **ai:** list the models of openai, xai, and google API keys (pi-rust extension) ([c02b11b](https://github.com/yannelli/pi-rust/commit/c02b11bbcf490e43812f59b5601945aef1c5240b))
- **durable:** durable code mode (upstream 7b597d148) ([76046ee](https://github.com/yannelli/pi-rust/commit/76046eeaac36f69d7cbbd626e77d5dc4fba2241c))
- **ai:** list the chat models of a github-copilot login (pi-rust extension) ([6b8f7c7](https://github.com/yannelli/pi-rust/commit/6b8f7c7eaa1a20bd2b7f8b400057308bf56957f2))
- **ai,coding-agent:** list the models of an Anthropic key or login (pi-rust extension) ([957acc8](https://github.com/yannelli/pi-rust/commit/957acc8cd9f19e34c8f439de5f0007c2b2839c87))
- **acp:** report turn token usage and session cost ([516ea20](https://github.com/yannelli/pi-rust/commit/516ea20981472f4685248a5b47512a7cea7c1250))
- **coding-agent:** wire reachesTools for JS extension tools and the tool listing (upstream 24382bedd) ([797a615](https://github.com/yannelli/pi-rust/commit/797a615bd89b3767a9e42ba4e50a4e30eaedd9ea))
- **ai:** list the models of the signed-in account for openai-codex ([67cb1cf](https://github.com/yannelli/pi-rust/commit/67cb1cf66acd794f9b69dd1aa98c1c90208b35f9))
- **ai,coding-agent:** list only the OpenRouter models a key may use (upstream ee409a85f) ([9e55318](https://github.com/yannelli/pi-rust/commit/9e55318bd17a643e9b5b5fde0f9dfc20c75ea43c))
- **pi-js:** add net listen reusePort on Linux and dgram source-specific multicast membership ([5ebec8e](https://github.com/yannelli/pi-rust/commit/5ebec8e43b8140f0bd9dca65d10042f866d63763))
- **pi-js:** send and read http2 custom settings, with SETTINGS ACKs driving localSettings and the settings() callback ([fae80c9](https://github.com/yannelli/pi-rust/commit/fae80c999da1b273ce763b5e63d68e6777c448bf))
- **pi-js:** add node:test snapshot assertions with Node's snapshot files and --test-update-snapshots ([2fca55c](https://github.com/yannelli/pi-rust/commit/2fca55c8b935d7370652b29efee16fdaf82bf29f))
- **env:** list_files for remote and local environments, node_platform (upstream b6fda579f) ([1261a26](https://github.com/yannelli/pi-rust/commit/1261a26fbcd8ec4d56c65d54ec9ed2098eed2de0))
- **pi-js:** step sqlite StatementSync.iterate() one row per next() on a cursor shared like Node's statement ([cfabfd6](https://github.com/yannelli/pi-rust/commit/cfabfd691e4ba84492ecec333032593a49e56878))
- **codemode:** show schema constraints in tool declarations (upstream 8a9c051f8) ([4ce2041](https://github.com/yannelli/pi-rust/commit/4ce2041f7bea7a7de25bea26fc0f89fd14062997))
- **codemode:** deterministic runs that the host answers (upstream 0fb1aee74) ([69d5c79](https://github.com/yannelli/pi-rust/commit/69d5c79b6e4cda13dd922a559f816c2dd89dc66c))
- **env:** add the listFiles operation to the daemon (upstream aa499c242) ([caf0a07](https://github.com/yannelli/pi-rust/commit/caf0a0798a1bd669084be2db88b5b5641c44df1d))
- **env:** report a terminal's foreground job with ptyForeground (upstream 74613de65) ([6df97fc](https://github.com/yannelli/pi-rust/commit/6df97fce0dbdb954caaa0cac493542b26c267120))
- **env:** add pty operations to the daemon (upstream 367b2bcf5) ([8c8d0f0](https://github.com/yannelli/pi-rust/commit/8c8d0f09efe429fadd54cc5a0751a83cfc51f0fb))
- **durable:** exclusive SQLite operations and a replicated connection wrapper (upstream 48c8f3228) ([0b4d001](https://github.com/yannelli/pi-rust/commit/0b4d001e07ce5979df86ff72a6e9079dbf1233b9))
- **pi-js:** support child_process stdio pipes above 2, file descriptors and fs streams, and TCP sockets as stdio entries ([b7d1d11](https://github.com/yannelli/pi-rust/commit/b7d1d11997aa48d85a94fb6c1fce671b099d50c5))
- **pi-js:** add readline terminal mode with line editing, history, and keypress parsing, used by the REPL ([af0afc0](https://github.com/yannelli/pi-rust/commit/af0afc0f26a9f7a63ca05c40a5d9d46a61fdd919))
- **pi-js:** parse multipart and urlencoded bodies in Body.formData and stream fetch request bodies ([cf624a6](https://github.com/yannelli/pi-rust/commit/cf624a661860d5f885c46ada4610595d058e2171))
- **pi-js:** add readable byte streams, BYOB readers, and Duplex.from for promises, functions, Blobs, and web streams ([0c246db](https://github.com/yannelli/pi-rust/commit/0c246dba6d1a632b7bd328db0c2d2c198bfcd806))
- **pi-js:** add fs fchmod, fchown, lutimes, statfs, lchmod, openAsBlob, and FileHandle chmod, chown, readableWebStream ([dad8ae0](https://github.com/yannelli/pi-rust/commit/dad8ae039dab9ede145a2cb7adcd215edb906a22))

### Fixed

- **js:** send the FIN before discarding unread bytes when a TCP socket closes ([3d5da00](https://github.com/yannelli/pi-rust/commit/3d5da000f48835fedc34721ba7ef45c898db8892))
- **ai,coding-agent:** drop models retired upstream (upstream e5fe9777b) ([4b29139](https://github.com/yannelli/pi-rust/commit/4b2913943d31aa53f08a1d3ca8ded7a254185444))
- **ai:** handle ChatGPT Business workspace usage limit errors (upstream cedefc532) ([f74748f](https://github.com/yannelli/pi-rust/commit/f74748f1085dadad42d260eaf051becac1c8e05f))
- **js:** start async host calls in the order JavaScript made them ([4f869f9](https://github.com/yannelli/pi-rust/commit/4f869f9ff8ea3518e4d3c1c67d301dc3af659669))
- **durable:** fail a run whose blocking compaction failed; keep compaction failures visible (upstream 89e5c2c07) ([648c61f](https://github.com/yannelli/pi-rust/commit/648c61f7f84efb58439f8ee877bd79a8f22a3ec9))
- **coding-agent,durable:** omit thinking from compaction summaries (upstream 15e7bdc14) ([ebce4de](https://github.com/yannelli/pi-rust/commit/ebce4de4bb0da06320c02f484a19307610aaabf2))
- **ai,coding-agent:** guard account model lists against redirects, unrelated lists, and upgrades ([73c3568](https://github.com/yannelli/pi-rust/commit/73c35687d7d70a7b958c9259bbdde1f028a26b68))
- **ai:** sync the model catalog with upstream model data (upstream fbc456d82) ([12509c0](https://github.com/yannelli/pi-rust/commit/12509c036127611b11f561dc81cb4d8858ef9b5d))
- **coding-agent:** restore terminal modes before process::exit in main and the config selector (upstream 9df819de3) ([eef6324](https://github.com/yannelli/pi-rust/commit/eef63249799a99ece3910f99a8360f5293013835))
- **durable:** detect the legacy WSL launcher from SystemRoot and fall back when it is empty (upstream 61060ddf8) ([ae3c129](https://github.com/yannelli/pi-rust/commit/ae3c12989c34bd72583f651ec68a23735e8be4c0))
- **coding-agent:** use tiered pricing and the model type check in the pi-ai compat helpers ([654a662](https://github.com/yannelli/pi-rust/commit/654a662714afadea7be900d82adaa20a98e371e3))
- **coding-agent:** warn and exit non-zero in print mode on truncated response (upstream 441f5767c) ([9d519e1](https://github.com/yannelli/pi-rust/commit/9d519e16939f959d9de096cad5d455332b8130cf))
- **coding-agent:** expand ~ in resource setting patterns (upstream 6a51a39d4) ([03516fb](https://github.com/yannelli/pi-rust/commit/03516fbda4ea52c5beea5470076507dd4d13f319))
- **coding-agent:** emit before_agent_start for runs started by custom messages (upstream a447ba11f) ([c00e605](https://github.com/yannelli/pi-rust/commit/c00e6057ed842fdd0c4a12b868eb6b091d07ed47))
- **coding-agent:** color the compaction indicator like the editor border when embedded (upstream dbc7ff87c) ([9c89aeb](https://github.com/yannelli/pi-rust/commit/9c89aebd656ab26702448ec35b3bd9c625cd8e24))
- **durable:** closing the code mode sandbox does not decide the script's outcome (upstream 04566631c) ([5f9574f](https://github.com/yannelli/pi-rust/commit/5f9574fcac3103ee64ad50b978135b6dd87ddf05))
- **js:** send a datagram at once when the OS takes it and report it on the next tick, so dgram send callbacks run before any reply as in Node ([6e377de](https://github.com/yannelli/pi-rust/commit/6e377de66ba912964df9827eb4b7128a7164c7af))
- **durable:** bound the scheduler's ownership indexes by live work (upstream ea448f454) ([2f2e3b7](https://github.com/yannelli/pi-rust/commit/2f2e3b7fe540a8ce75d64b7008eabba210d68d98))
- **js:** free a keep-alive client socket when the response ends after the request was fully handed to it, so a busy runtime still reuses it as Node does ([dc8a619](https://github.com/yannelli/pi-rust/commit/dc8a6199aaa5d1d1ce8cf9b659987715f7d47d58))
- **durable:** drop redundant nested-call parent check; test owned_tasks() with a held child (upstream e37cdc420) ([219bc54](https://github.com/yannelli/pi-rust/commit/219bc54c7b39bb54823fd37d2add4652a085640f))
- **codemode:** reject a misplaced @options line (upstream b6d505cd1) ([50bd9a8](https://github.com/yannelli/pi-rust/commit/50bd9a84b7017a0430348eaba7cb05ab9ef9194e))
- **mcp:** accept any JSON value as tool structuredContent (upstream 67996ab75) ([b7446ba](https://github.com/yannelli/pi-rust/commit/b7446ba4b11e1a5d0dd2ebb4f0558bbf5b03b31f))
- **parity:** harden the daily workflow and the report ([aa6932a](https://github.com/yannelli/pi-rust/commit/aa6932aa4408d7b7f86a3b5273db358e4d5cc106))
- **mcp:** stop listing MCP resources when connecting (upstream f4cca2dc2) ([8238187](https://github.com/yannelli/pi-rust/commit/8238187bfc8f3b8b108b368fcec60b331a85883e))
- **mcp:** describe pi mcp login --timeout as a limit on the whole sign-in (upstream 148622bc6) ([472aa31](https://github.com/yannelli/pi-rust/commit/472aa31abde3011120d6ff1a35b2cc6fbea58231))
- **coding-agent:** accept mcp.json files with a UTF-8 BOM (upstream bdd9f0399) ([aad7428](https://github.com/yannelli/pi-rust/commit/aad7428419ca5e672261e29dd955a089a1b69b04))
- **coding-agent:** expose @earendil-works/pi-ai/models in the extension loader (upstream 22dc6c26f) ([694dc43](https://github.com/yannelli/pi-rust/commit/694dc43964208659aa56fa6330fda1533a179a05))
- **coding-agent:** queue codemode scripts at the worker limit (upstream 885a564b3) ([299d8ff](https://github.com/yannelli/pi-rust/commit/299d8ffe3cf6bad8e6df22e92cd143ac73515276))
- **durable:** scheduler no longer visits every live task on each pass (upstream cb4375493) ([1418cce](https://github.com/yannelli/pi-rust/commit/1418cced3fcd0cb32d8808495d49687fb49f6750))
- **coding-agent:** keep clipboard helpers out of pi's process group so Ctrl+Z does not stop them (upstream f8ab362cc) ([d238187](https://github.com/yannelli/pi-rust/commit/d2381870811389d1d06047fcdbe95e228ba3b5bb))
- **coding-agent:** stack concurrent extension dialogs instead of replacing them (upstream 38847563f) ([10d464b](https://github.com/yannelli/pi-rust/commit/10d464b70ff389fc39c243e4ba17fb19741c6118))
- **codemode:** description says schema tools resolve with their value on failure (upstream 427052247) ([9acd33c](https://github.com/yannelli/pi-rust/commit/9acd33ce454897017e4f1b84e7cea91396d4730c))
- **mcp:** reject iss when no authorization server metadata was discovered (upstream c5f5b3282) ([750837d](https://github.com/yannelli/pi-rust/commit/750837d596172b22e47221c03e8360619a7a0b0c))
- **coding-agent:** render CJK emphasis in HTML export (upstream 26293e74f) ([9835aee](https://github.com/yannelli/pi-rust/commit/9835aee68001324eccef6bd735999d573b928a99))
- **coding-agent:** resolve Windows shells from installation directories (upstream 61060ddf8) ([76cfa90](https://github.com/yannelli/pi-rust/commit/76cfa90d533942cabfe43f098a3c949d03ba568b))

## [0.5.3] - 2026-10-21

### Added

- **tui:** enable Kitty images for Rex (upstream 396ecd7b8) ([18e9ae8](https://github.com/yannelli/pi-rust/commit/18e9ae8756c266cb89c0671449a1cb97e0d4fb77))
- **tui:** detect Rex terminal for OSC 8 hyperlinks (upstream 0ed1b0431) ([0012227](https://github.com/yannelli/pi-rust/commit/0012227a7583225b62293d4cf918926456840a62))

### Fixed

- **ai:** report malformed final tool call arguments for OpenAI APIs (upstream 87f9a3641) ([b857953](https://github.com/yannelli/pi-rust/commit/b857953935d31c1916a458916a20103fde6cf35c))
- **ai:** cap Windows delete-pending lock retries separately (upstream e6fb2581d) ([de3144a](https://github.com/yannelli/pi-rust/commit/de3144ae731509c326fe23337acbed50b53d39b6))
- **ai:** fall back on Anthropic strict tool limits (upstream d4d53612b) ([ac2ad28](https://github.com/yannelli/pi-rust/commit/ac2ad282ef285d38b81c1056574b8e967896a87c))
- **tui:** let unhandled wheel input scroll the transcript under a focused overlay (upstream 11f991145) ([de25ac8](https://github.com/yannelli/pi-rust/commit/de25ac839586293d77d00959da8b5b5ed9995fd1))
- **tui:** enable kitty images in herdr (upstream 78a3c10ba) ([27e4b23](https://github.com/yannelli/pi-rust/commit/27e4b234909cca818e3f84c92f50b6e1fdbd6546))
- **tui:** keep delayed terminal replies out of keyboard input (upstream edb2eb966) ([3902bef](https://github.com/yannelli/pi-rust/commit/3902bef441f6d100a49902fdf43fffc32202c33c))
- **tui:** restore terminal modes when the process exits without stop() (upstream 9df819de3) ([8b0589b](https://github.com/yannelli/pi-rust/commit/8b0589b54ad3f033588c51599692afe91364b3b5))
- **tui:** render CJK emphasis next to punctuation (upstream b2d0da6fe) ([4089fb3](https://github.com/yannelli/pi-rust/commit/4089fb3053aee12cb5ccd631dea16c840b9c0e2b))
- **tui:** stop unclosed display math from swallowing Markdown tables (upstream 2ed3118b0) ([0c95c81](https://github.com/yannelli/pi-rust/commit/0c95c81197adecf3ae03603b928ae819175bf80b))
- **ai:** drop Codex models the backend rejects for OAuth accounts (upstream fdce48eb4) ([7e98579](https://github.com/yannelli/pi-rust/commit/7e98579fac569f023f235e3ae901780754225304))
- **ai:** only list announced removals as still unavailable (upstream 530015d93) ([e7fd367](https://github.com/yannelli/pi-rust/commit/e7fd367bc6e691046cf19feb0f81537cb40bd84e))
- **ai:** keep the prompt cache across tool changes on Responses (upstream caa6967b6) ([9500d62](https://github.com/yannelli/pi-rust/commit/9500d626663514644468eb97afce77383b7d612e))
- **ai:** recover from Copilot request-size overflow (upstream 73f8de80b) ([460832c](https://github.com/yannelli/pi-rust/commit/460832c07287901ed3044597a753220884fb10ab))

## [0.5.2] - 2026-10-21

### Added

- **xtask:** add the parity report ([1688647](https://github.com/yannelli/pi-rust/commit/16886472405346d7e19f3e81e92f0752aa60b361))

### Fixed

- **xtask:** count node:-only modules on Node 22 and keep upstream model data ([9e8cff5](https://github.com/yannelli/pi-rust/commit/9e8cff5079424f5dc5e1cf09f81e9a0afee7595f))
- **xtask:** skip the model catalog when upstream model data is not generated ([bcbcde0](https://github.com/yannelli/pi-rust/commit/bcbcde02d9457f0a9e4e46c4ccb0ff768f3b23bb))

## [0.5.1] - 2026-10-21

### Fixed

- **coding-agent:** remove the Anthropic subscription extra-usage warning ([d50e0a7](https://github.com/yannelli/pi-rust/commit/d50e0a73381657798e0ea70773051cd681fb02c5))

## [0.5.0] - 2026-10-20

### Breaking Changes

- **ai:** add pi.ai.request and pi.ai.poll telemetry spans ([0f402d8](https://github.com/yannelli/pi-rust/commit/0f402d8d5c27c98773867d8f2757c467071857b8))

## [0.4.0] - 2026-10-20

### Breaking Changes

- **durable:** emit content-free harness telemetry ([13c5895](https://github.com/yannelli/pi-rust/commit/13c589596fec097b4d1320ee69fe1d8e28f0a3e8))
- **telemetry:** add propagation, span links, and metrics; Context::telemetry ([da0804d](https://github.com/yannelli/pi-rust/commit/da0804de82fa6ad25ca913d9926ccc7a5d7053fa))

## [0.3.0] - 2026-10-20

### Breaking Changes

- **durable:** crash-loop guard, typed reports, and inputs after failed runs ([7acb086](https://github.com/yannelli/pi-rust/commit/7acb086ae37810d9fd99263a1e30e2f5afe98392))
- **durable:** durable sleeping state replaces runtime.sleep ([cc7380b](https://github.com/yannelli/pi-rust/commit/cc7380b15ac811ff15617ab1367b79fc607c14fb))

## [0.2.4] - 2026-10-19

### Fixed

- **js:** deliver an HTTP/2 session's last frames before it reports closing ([9892f9b](https://github.com/yannelli/pi-rust/commit/9892f9b6277bbbee4364b9b425bdd8f4e0488bc9))

## [0.2.3] - 2026-10-19

### Fixed

- **interactive:** report ready only after the first extension bind ([dee9c05](https://github.com/yannelli/pi-rust/commit/dee9c0530f9b91cb74f055aca2cf63427106dfaa))

## [0.2.2] - 2026-10-19

### Added

- **js:** report os.cpus times from /proc/stat, host_processor_info, and NtQuerySystemInformation ([c11336c](https://github.com/yannelli/pi-rust/commit/c11336c410d5be8430759f24542b9ec80377d843))
- **js:** share SharedArrayBuffer memory between workers and let Atomics.wait block ([dd1c022](https://github.com/yannelli/pi-rust/commit/dd1c022ee31e9416bc7fe6be0cc4c37e70993f7b))
- **js:** add HTTP/2 server push and read the peer's SETTINGS frames ([47a07ca](https://github.com/yannelli/pi-rust/commit/47a07ca64505d1965f31fc61fc82d62e54316dec))
- **js:** add TLS over existing sockets and streams, SNICallback and addContext, and client session resumption ([161b0e3](https://github.com/yannelli/pi-rust/commit/161b0e3fe6614b6fe519bc259fce28f734aa1a8f))
- **js:** connect to looked-up addresses with Node 22's autoSelectFamily rules and events ([2fedbed](https://github.com/yannelli/pi-rust/commit/2fedbed807dd3677482dce9a04f3d5591f76d6c8))
- **js:** pass the real stdio descriptors to children with stdio 'inherit' when no output sink is set ([25764c5](https://github.com/yannelli/pi-rust/commit/25764c53b2f8f4858886e5c1a3ccdd814756d9dd))
- **js:** decode gzip, deflate, and brotli fetch bodies and send accept-encoding like undici ([63d253e](https://github.com/yannelli/pi-rust/commit/63d253e0c9e095164a454fe0fc553f92a03b5781))
- **js:** add node:wasi and the WebAssembly global on the wasmi interpreter, with resumable calls into JavaScript imports and shared memory buffers ([e332e3a](https://github.com/yannelli/pi-rust/commit/e332e3a571d6e709bd21d3b85441ebe630d51941))
- **js:** add node:inspector and node:inspector/promises with in-process Runtime, HeapProfiler, and Schema sessions ([2999a61](https://github.com/yannelli/pi-rust/commit/2999a617d64fb36b8e16835171be48161feb3982))
- **js:** add node:repl with vm contexts, multi-line input, dot commands, custom eval and writer, and top-level await ([bba6b5d](https://github.com/yannelli/pi-rust/commit/bba6b5d0e9960e621e6eb69dd31ba2406107b356))
- **js:** add node:test and node:test/reporters with suites, hooks, mocks, mock timers, run(), and TAP, spec, dot, and junit output ([879e38e](https://github.com/yannelli/pi-rust/commit/879e38ee12f7fd2a6b8754920a82b4dadc08de34))
- **js:** add node:sqlite with DatabaseSync, StatementSync, user functions, aggregates, and backup on the bundled SQLite ([9cbb6e8](https://github.com/yannelli/pi-rust/commit/9cbb6e83a84d3b3be09d5a6ee5ba7cddffc38844))

### Fixed

- **js:** keep an HTTP/2 session's socket writable after the peer half-closes ([bc13f0c](https://github.com/yannelli/pi-rust/commit/bc13f0ce6a9520aa9feba34f7faf7716521aa014))
- **js:** report a missing .load file argument like Node ([842a13f](https://github.com/yannelli/pi-rust/commit/842a13f716491498aa496d7d7c8b6831a9cd0bce))
- **js:** refuse to transfer WebAssembly.Memory buffers ([31a9705](https://github.com/yannelli/pi-rust/commit/31a9705a010821dd9947b53660001f9384d579ee))
- **js:** emit 'end' on sockets nothing reads and ignore shutdown failures like Node ([d773bde](https://github.com/yannelli/pi-rust/commit/d773bde3a12767ba3d2ce44dbe91b49ed60751ab))
- **js:** discard unread bytes before closing a TCP socket ([272a122](https://github.com/yannelli/pi-rust/commit/272a12200a55d791dcf75b121e106f7e676f69f7))

## [0.2.1] - 2026-10-19

### Fixed

- **experimental:** flush plugin package profile writes ([f5f9e13](https://github.com/yannelli/pi-rust/commit/f5f9e13a9ed6e6205992602ca3646d54e5d5b59c))

## [0.2.0] - 2026-10-19

### Breaking Changes

- **durable:** support namespaced SQLite storage ([a23c349](https://github.com/yannelli/pi-rust/commit/a23c349ff22f3ffacbc62763457077eb5f818b71))
- **ai:** remove the unused assistant-message frame API ([1df04eb](https://github.com/yannelli/pi-rust/commit/1df04ebeab79b11c5a9184c686ef17b481d4071e))

### Added

- **durable:** per-model compaction settings via compaction model overrides ([f427319](https://github.com/yannelli/pi-rust/commit/f42731908ef86238a805fa2606380d3cfb5fed7f))
- **coding-agent:** configure the session share destination ([fc93f71](https://github.com/yannelli/pi-rust/commit/fc93f711bf3a856a870594ff4e6152bc7d2c06e7))
- **ai:** refresh the model catalog from upstream pi ([f34e4f3](https://github.com/yannelli/pi-rust/commit/f34e4f36c65578f86ddf1ffeb3ad363fc33a7f20))
- **ai:** keep the prompt cache when changing thinking level on OpenAI GPT-6 ([404bd78](https://github.com/yannelli/pi-rust/commit/404bd7862eeed64224d37d00b3dbdda44a2d634a))

### Fixed

- **durable:** lead with the system baseline after a compaction that keeps assistant messages ([c8b3381](https://github.com/yannelli/pi-rust/commit/c8b33819990fd0951d024c3b7f22bb8cf90561d2))
- **coding-agent:** resolve model references through one provider selection policy ([e4bd988](https://github.com/yannelli/pi-rust/commit/e4bd988044854c32116ea8c9ede630cdb064340c))
- **coding-agent:** describe models.json fields and use pi.dev schema URLs ([fd3aabf](https://github.com/yannelli/pi-rust/commit/fd3aabf0ede47c7ff863c50fdcb6d6b6e0e9d440))
- **codemode:** bound live codemode workers across calls and sessions ([e247d66](https://github.com/yannelli/pi-rust/commit/e247d66bf147eddafd980f8259d224faf796d35b))
- **coding-agent:** restore active tools on session resume ([d6103de](https://github.com/yannelli/pi-rust/commit/d6103de78b421d6c90f69236cf8535da354212db))
- **coding-agent:** normalize read line-range labels ([052708f](https://github.com/yannelli/pi-rust/commit/052708f779162557e764708ecb957f9aa18c4ef0))
- **coding-agent:** keep empty quoted prompt template arguments ([da7f364](https://github.com/yannelli/pi-rust/commit/da7f36479be8cbd1adf17e132777996a2b74b895))
- **coding-agent:** remove fixed output limits from summary requests ([f6f96de](https://github.com/yannelli/pi-rust/commit/f6f96deb5a0c61368da332a05abadf75bb12f592))
- **ai:** port provider fixes from upstream pi 1.3.0 ([f8df167](https://github.com/yannelli/pi-rust/commit/f8df167f2c388dfd127b76752e2d21e627597dda))
- **ai:** report malformed finished tool-call arguments and keep invalid \u escapes ([19d0270](https://github.com/yannelli/pi-rust/commit/19d02707ed0ed480834a0420b8bd205cabf7276f))
- **ai:** retry more transient gateway and fetch errors ([a7d6676](https://github.com/yannelli/pi-rust/commit/a7d6676529f5fc101ad89b357d83a337e0932bfb))

## [0.1.11] - 2026-10-14

### Added

- **js:** add node:cluster with round-robin and shared servers, and pass TCP sockets and servers over IPC ([b45f092](https://github.com/yannelli/pi-rust/commit/b45f09262e6bece10e53393614a7443c69af2b04))
- **js:** add node:worker_threads with workers on their own runtime threads, cross-thread message ports, and BroadcastChannel ([1c3800f](https://github.com/yannelli/pi-rust/commit/1c3800fadd0ebd0b76f739b83007a048c0795c35))
- **js:** add node:vm with separate QuickJS contexts, interrupt-based timeouts, and compileFunction ([c059cd0](https://github.com/yannelli/pi-rust/commit/c059cd075421225387682d87b85eb1652c850df8))
- **js:** add node:http2 sessions, streams, and the compat API on the h2 crate over net and tls sockets ([81a854a](https://github.com/yannelli/pi-rust/commit/81a854a6802725749d139bc224c9746ccfe7cb26))
- **js:** add node:http and node:https in JavaScript over net and tls sockets, with an httparse head parser ([8d7a7be](https://github.com/yannelli/pi-rust/commit/8d7a7be4c798bdb7b9adfca159d637ca9ba181f7))
- **js:** add node:tls on rustls with ALPN, SNI, custom CAs, and Node's hostname checks ([22f8808](https://github.com/yannelli/pi-rust/commit/22f88086f72167990774ab68cd9bf10234f841dc))
- **js:** add node:dgram UDP sockets, with dgram and dns tests on localhost ([6f55b26](https://github.com/yannelli/pi-rust/commit/6f55b267555819efb305acb181cef76558ece3f2))
- **js:** add node:dns and dns/promises with system lookup and hickory-resolver queries ([2f53c9d](https://github.com/yannelli/pi-rust/commit/2f53c9dcff6dedb2bb786cc2230e5d34d9d891f0))
- **js:** add node:net sockets and servers over TCP, Unix sockets, and named pipes ([38c142d](https://github.com/yannelli/pi-rust/commit/38c142dc88f5e0e9ef102d38f8c1ffdd22e6c9ba))
- **js:** move the typed array byte copy into convert, keeping zlib.rs under 480 lines ([6875294](https://github.com/yannelli/pi-rust/commit/6875294f1e2a935868af05c0e805342357700c84))
- **js:** list zlib, async_hooks, and v8 in the README's Node support line ([d7be223](https://github.com/yannelli/pi-rust/commit/d7be2237817a35f8b2854aae38dee0dd4d2a22cb))
- **js:** add node:v8 with V8 wire-format serialize and deserialize, and heap statistics from QuickJS ([b8f6abd](https://github.com/yannelli/pi-rust/commit/b8f6abde399d11483c6cded5aad7973490c3af19))
- **js:** add zlib (deflate, gzip, brotli, zstd, crc32), async_hooks with async context, domain, punycode, sea, and trace_events ([851c4dd](https://github.com/yannelli/pi-rust/commit/851c4dd4ee6487f3afdedd4b5240e5588d6ba279))

### Fixed

- **js:** check the platform when IPC messages flow, since a forked child loads internal/ipc before process is built ([2564b08](https://github.com/yannelli/pi-rust/commit/2564b08149ee5a0f8cf2b089f274449c84f4877d))
- **js:** close listeners at once, defer reads on accepted sockets, ack passed handles, and end Windows pipes like libuv ([8a315e3](https://github.com/yannelli/pi-rust/commit/8a315e36f475a866237a6e7024b6dec0b730464f))

## [0.1.10] - 2026-10-14

### Added

- **js:** move node:crypto onto RustCrypto crates and add AES modes, DES, DSA, Ed448, X448, encrypted keys, and util.MIMEType ([9d30a36](https://github.com/yannelli/pi-rust/commit/9d30a36ea8ed776e000afd781c310bdf734320f1))
- **js:** implement child_process.fork with IPC, process.stdin and setRawMode, tty.ReadStream, native fs.watch, os priorities and interfaces, cpuUsage, resourceUsage, and abort ([19cc389](https://github.com/yannelli/pi-rust/commit/19cc38992e7cba9b50525e295bddc7803e34c54f))
- **js:** implement parseArgs, system error names, matchesGlob, fs.glob, cp filters, bigint Stats, perf_hooks timing, tracing channels, stream.compose, transcode, blob URLs, CallTracker, and module hooks ([e34eea8](https://github.com/yannelli/pi-rust/commit/e34eea8067ea40dd41acf9436217993d9d2717e2))
- **js:** implement node:crypto keys, signatures, AEAD ciphers, DH, ECDH, scrypt, hkdf, and primes ([2036fb1](https://github.com/yannelli/pi-rust/commit/2036fb1883bbf4905e551fa549833d0ba708a071))

### Fixed

- **js:** hold IPC messages until a message listener exists, and re-arm intervals from when they run ([4a20c2f](https://github.com/yannelli/pi-rust/commit/4a20c2f8afbdb0ae8dc263af62f35168e24984ae))
- **js:** report a macOS file write as change in fs.watch, and time out a stuck fork test with its step ([ef06722](https://github.com/yannelli/pi-rust/commit/ef067225667bc5df6ad2332280193d343fe93cae))

## [0.1.9] - 2026-10-14

### Added

- **evals:** gate Unix-only container paths for Windows builds ([95c2975](https://github.com/yannelli/pi-rust/commit/95c29751371e3cba652e0c14edc39744fbe3625d))
- **evals:** add the docs comparison CLI, container entrypoint, and Docker image ([31c31a1](https://github.com/yannelli/pi-rust/commit/31c31a1cf6de02b792820155159df96d30b44698))
- **evals:** add the pi harness, eval framework, suites, and fixtures ([18ecaf7](https://github.com/yannelli/pi-rust/commit/18ecaf71ba6c81fc3cabaea06d9df5ef810b2e3a))
- **evals:** add pi-evals crate with plan and report logic ([00df278](https://github.com/yannelli/pi-rust/commit/00df27819610e2aedb8a1ac56cc51e98b23a9a7c))

## [0.1.8] - 2026-10-14

### Added

- **experimental:** get the client TUI's server service handles before it waits for readiness ([f78f0ce](https://github.com/yannelli/pi-rust/commit/f78f0ce848e5c930f02ba2df75d1c0074729abf1))
- **experimental:** port the client, client runtime, client TUI, and the pi experimental command ([4592143](https://github.com/yannelli/pi-rust/commit/4592143f25ad9272373c5d967f82033802bfdb28))
- **experimental:** port the durable TUI shared by durable/ and vacation/ ([7e9dc2a](https://github.com/yannelli/pi-rust/commit/7e9dc2a25fd8327bea889f2368de3463a7af6416))
- **experimental:** port the Radius relay host, client transport, reconnect, and auth resolver ([5307c4c](https://github.com/yannelli/pi-rust/commit/5307c4ce0507fb70387837c95f6ad7f43ae43c91))
- **experimental:** port durable/, vacation/, and the coding agent Session worker Harness ([db07608](https://github.com/yannelli/pi-rust/commit/db07608b813f58816862d6b5a6798a36f185cecb))
- **experimental:** port the experimental server, coordinator, Session workers, plugins, and services ([37cd35d](https://github.com/yannelli/pi-rust/commit/37cd35d1b595d692d4fa3e02b054d791a81ee6e2))

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
