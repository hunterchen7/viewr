# Changelog

## [0.3.0](https://github.com/hunterchen7/viewr/compare/v0.1.1...v0.3.0) (2026-07-29)

This is the first public release after v0.1.1. The v0.2.0 draft was not
published, so these notes cover the complete public upgrade.

### Highlights since v0.1.1

* Native macOS, Windows, and Debian installers, ARW/MIME integration, portable archives, and a vendored source archive.
* Verified in-app updates with release notes and Download now, Later, and Skip this version choices.
* Restored window size and position plus configurable loading text, image information, and loading indicators.
* Full-resolution preloading for the current and adjacent images, with viewport-first tiled loading while zoomed.
* Selectable JPEG cache quality, faster libjpeg-turbo encoding, parallel restart-marker decoding, and better dark-gradient preservation.
* Brighter, highlight-safe RAW development and extensive cache, decode, rating, XMP, and sidecar-persistence hardening.

### Breaking changes carried forward from the unpublished draft

* **db:** `record_rating_pending_sidecar` now rejects paths without a resolvable physical sidecar owner instead of journaling unresolved work. `Db::open` also requires WAL-capable storage, and `DbError` adds `WalUnavailable`.

### Features

* add clickable star markers to the filmstrip scrollbar ([a14d411](https://github.com/hunterchen7/viewr/commit/a14d411a2bcb2dab5c9a599c139ef64e2e92b7a2))
* add configurable image-processing thread limits ([805a6b8](https://github.com/hunterchen7/viewr/commit/805a6b88da8c96edce985206ad09cf0316575c5f))
* add optional vertical filmstrip scrolling ([0295134](https://github.com/hunterchen7/viewr/commit/0295134301a7aac542b7c615b79675b48fc4c3cb))

### Bug fixes

* **ci:** harden release recovery and publication ([448c2c7](https://github.com/hunterchen7/viewr/commit/448c2c79333b061675e778351832808da53921c2))
* **ci:** prefetch the complete license graph ([#22](https://github.com/hunterchen7/viewr/issues/22)) ([cd59c8f](https://github.com/hunterchen7/viewr/commit/cd59c8f2cf3adf9b1f803f27480a01bd5c47e587))
* keep preferences controls reachable ([ca9152c](https://github.com/hunterchen7/viewr/commit/ca9152c2b1bb7792eb225433e06790e5e97e038f))
* **packaging:** ship required JPEG attribution ([c846ac4](https://github.com/hunterchen7/viewr/commit/c846ac4d9448630e4dade1f4ba98899efbbedb44))

## [0.2.0](https://github.com/hunterchen7/viewr/compare/v0.1.1...v0.2.0) (2026-07-28)

This version remained a draft and was never published. Its user-facing changes
are summarized in v0.3.0, the next public release.

### ⚠ BREAKING CHANGES

* **db:** record_rating_pending_sidecar now rejects paths without a resolvable physical sidecar owner instead of journaling unresolved work. Db::open also requires WAL-capable storage and DbError adds WalUnavailable.

### Features

* add installer launch and release source support ([6e76d3c](https://github.com/hunterchen7/viewr/commit/6e76d3cfd37c60cd05879ac66d44e5a211648f75))
* add validated native installers to releases ([2b961e9](https://github.com/hunterchen7/viewr/commit/2b961e9b37794bb1ea8298c4154791e1c229110f))
* add verified in-app updates ([2b48ac5](https://github.com/hunterchen7/viewr/commit/2b48ac537d5f8849b4336f60381df1d6977f0363))
* add verified in-app updates ([42d0ec4](https://github.com/hunterchen7/viewr/commit/42d0ec495b4bb1a3676546b31c6a90300fbcb8c9))
* establish updater foundations ([9f129a4](https://github.com/hunterchen7/viewr/commit/9f129a4a4c6ce6bc8c80eb9a6d69022b2d1639df))
* expose cache JPEG quality preference ([9cebb8b](https://github.com/hunterchen7/viewr/commit/9cebb8baff8fef600b7fa6084383045d1b81a99b))
* isolate selectable JPEG cache profiles ([fba8d88](https://github.com/hunterchen7/viewr/commit/fba8d8800f6a91e261fc8b731e51e32c534c3703))
* **linux:** add Debian installer ([24dc547](https://github.com/hunterchen7/viewr/commit/24dc547c32bb1137315deb13859344cf13bfde45))
* **linux:** add install and MIME integration checks ([bbdfbed](https://github.com/hunterchen7/viewr/commit/bbdfbed1e33fbd448c6cfe5a790049db91854323))
* load full textures viewport first ([67e16e3](https://github.com/hunterchen7/viewr/commit/67e16e3eaaf8eaa7d37030ce780ce6aa101f6a48))
* **macos:** add native package installer ([bc53079](https://github.com/hunterchen7/viewr/commit/bc53079aabd0099512ee105c1fe6abb9afcc74e2))
* **raw:** add highlight-safe culling exposure ([b1d8176](https://github.com/hunterchen7/viewr/commit/b1d81761ccc8d34646c53a982dc65f2c0a350210))
* **ui:** persist windows and customize indicators ([65c12b6](https://github.com/hunterchen7/viewr/commit/65c12b6c67185d1a06c04928e04472535e93a4cb))
* **windows:** add native MSI installer ([19d9a00](https://github.com/hunterchen7/viewr/commit/19d9a001dcd293863d248a3f800eb51c1ef4cbdf))


### Bug Fixes

* bind ratings to native raw identity ([6e4972c](https://github.com/hunterchen7/viewr/commit/6e4972c27c6f4af415c230296705a96b8d3aebe2))
* **ci:** harden installer integration validation ([6b6738f](https://github.com/hunterchen7/viewr/commit/6b6738f36ee34f6e1134845db6a8cea4c6ca8bc1))
* constrain disk cache garbage collection ([b0ca7c8](https://github.com/hunterchen7/viewr/commit/b0ca7c8fe42820064dc927f33095008e08b73622))
* contain panics in decode workers ([57af577](https://github.com/hunterchen7/viewr/commit/57af577c9ac00c99e428f31deba063fb17ce4471))
* **db:** avoid redundant concurrent repair ([1995d3b](https://github.com/hunterchen7/viewr/commit/1995d3bfd05589e6a231c075effe5b3423c3c982))
* **db:** guard delayed rating journals with revisions ([8e8bdba](https://github.com/hunterchen7/viewr/commit/8e8bdba91e367c13d8af3559d1b8f933656f6155))
* **db:** harden rating recovery across filesystem aliases ([af1b5b0](https://github.com/hunterchen7/viewr/commit/af1b5b011257391d4abee830993bea1e0cb7ee7f))
* **db:** preserve legacy owner ordering ([110b8c7](https://github.com/hunterchen7/viewr/commit/110b8c71ceba77723b9f9462e8f736a41b869ed7))
* **db:** scope recovery ordering by sidecar owner ([0c69539](https://github.com/hunterchen7/viewr/commit/0c695391f1c6e0d28c696ae31286b44e9330b346))
* **db:** separate rating generation from completion ([dd36d5d](https://github.com/hunterchen7/viewr/commit/dd36d5df5788f8248637fdf6c12f034f706a2442))
* **db:** serialize repair and legacy ambiguity ([235339a](https://github.com/hunterchen7/viewr/commit/235339abe362513bf6661e339a343f3dee90525b))
* fail closed on malformed RAW metadata ([8bdaa31](https://github.com/hunterchen7/viewr/commit/8bdaa31cb20416598aaac039ab5b7f2f6bfb608d))
* **folder:** match sidecar owners to filesystem identity ([4d9fd4a](https://github.com/hunterchen7/viewr/commit/4d9fd4ae50db5e863025058fce3e607548e5c56b))
* guard sidecar recovery with raw identity ([9813af7](https://github.com/hunterchen7/viewr/commit/9813af7bcd6d59f07fc5e1ca4e5f420942d03519))
* harden updater handoff and coordination ([44c790f](https://github.com/hunterchen7/viewr/commit/44c790f67bff7f5954bd0dc4aac9be35d2b5ad56))
* honor the total RAM cache budget ([415f28d](https://github.com/hunterchen7/viewr/commit/415f28d9a58de2ff016e08382f458352d03355e4))
* **jobs:** linearize events with worker generations ([393c1eb](https://github.com/hunterchen7/viewr/commit/393c1eb3a593abdd3ce7c7885052ac225e454473))
* **jpeg:** harden native release boundary ([98a27de](https://github.com/hunterchen7/viewr/commit/98a27de0e4614c138f5a13810983829b5956ab7d))
* **jpeg:** isolate background encoding work ([64e6967](https://github.com/hunterchen7/viewr/commit/64e6967de82ac7452dbe873f6cdfe2dc132eb037))
* **library:** make rating persistence ownership-safe ([65c3d2c](https://github.com/hunterchen7/viewr/commit/65c3d2cd9ab9b9f1b3973747ef1a51cca8a29c6a))
* **library:** retain configured database authority ([22059ad](https://github.com/hunterchen7/viewr/commit/22059adf04aa26c6e2b0c383bdd57186591e3825))
* **library:** unify sidecar ownership across aliases ([d31ab63](https://github.com/hunterchen7/viewr/commit/d31ab63b6506807461c9bce2af74095056ddc36e))
* **macos:** preserve batched open semantics ([e87f814](https://github.com/hunterchen7/viewr/commit/e87f814f9ed6ee3534613510705388a362efc32c))
* **macos:** preserve explicit ARW defaults ([d4e10a1](https://github.com/hunterchen7/viewr/commit/d4e10a17fa6a122509763f262ada1c8b9c28ec28))
* **macos:** preserve regular viewer integration ([094e9e2](https://github.com/hunterchen7/viewr/commit/094e9e2958f71fdee1f39a474738861433f9a662))
* **macos:** test Finder-equivalent ARW routing ([072bb2c](https://github.com/hunterchen7/viewr/commit/072bb2c03ffec1c7b2309f2ba73b71779ea5d6a4))
* **packaging:** align installer contracts ([b9ee5bf](https://github.com/hunterchen7/viewr/commit/b9ee5bf6c79b0682b4fb46e0308f669db5dd8500))
* **packaging:** clean up interrupted installer tests ([8fa2a66](https://github.com/hunterchen7/viewr/commit/8fa2a66de336fe072aca7f1eb567ac8d6dddf4e9))
* **packaging:** enforce archive platform invariants ([5fbeed8](https://github.com/hunterchen7/viewr/commit/5fbeed8d38948345dbae84d5f5b4d1e52f951353))
* **packaging:** make installer validation portable ([28c8f14](https://github.com/hunterchen7/viewr/commit/28c8f1467354965d86c5b1e3b167880319007393))
* pin version 5 JPEG cache identity ([908f982](https://github.com/hunterchen7/viewr/commit/908f982a18d07fb2e732d69ad2365a9251a096c3))
* preserve dark gradients in cache JPEGs ([87a4ba9](https://github.com/hunterchen7/viewr/commit/87a4ba990cacb4c396c6f284fa98c90cf53d7791))
* preserve database API compatibility ([936a974](https://github.com/hunterchen7/viewr/commit/936a974e53e1b2904bd1aa5e36114bc159fac108))
* preserve sidecar replacement boundaries ([dfc1844](https://github.com/hunterchen7/viewr/commit/dfc1844ed36b63c89791c26d61830807fcbc7d77))
* reject non-finite configuration values ([c9a2582](https://github.com/hunterchen7/viewr/commit/c9a2582adbc1a033d713860b76e039fa708d43ef))
* **release:** bind artifacts to the release tag ([4651661](https://github.com/hunterchen7/viewr/commit/465166178cbd5218dabc1c304d857db0b6e63ed0))
* **release:** pin jobs to the gated commit ([03fdc25](https://github.com/hunterchen7/viewr/commit/03fdc258029921932503c01751189b4335b8ba05))
* **release:** publish only validated installer assets ([e6e9236](https://github.com/hunterchen7/viewr/commit/e6e923616fb45c0d5833777edcb4f7c8cbcd8010))
* remove LRU map entries in release builds ([a90e7e6](https://github.com/hunterchen7/viewr/commit/a90e7e64741c21a8f7f180daf560a4dc2e014518))
* retain ratings until journaling succeeds ([2488ddc](https://github.com/hunterchen7/viewr/commit/2488ddc033a37e6bd1941e47987f74f73fd00d93))
* serialize sidecar publication ownership ([79440d6](https://github.com/hunterchen7/viewr/commit/79440d66be28d599a795976c6a17a530f8eb01d3))
* suppress stale worker panic events ([6d8709f](https://github.com/hunterchen7/viewr/commit/6d8709fa04c7f8894ee173569a1da3fd4f1aae37))
* **ui:** limit persistence to window geometry ([7d23a3a](https://github.com/hunterchen7/viewr/commit/7d23a3ab456e2fb6a2289a65062c4e127db5e685))
* **windows:** make installer components ICE-clean ([554c3d5](https://github.com/hunterchen7/viewr/commit/554c3d5db6bc44a71e587dfaa85f28834d57a7f2))
* **windows:** remove duplicate WiX UI property ([476f54e](https://github.com/hunterchen7/viewr/commit/476f54ef1a53d94de8dda138af31e6a272deaf12))


### Performance Improvements

* **cache:** preload full-resolution neighbors ([a8ff878](https://github.com/hunterchen7/viewr/commit/a8ff8785528e4be69c630dc1cd25697ea7986715))
* **db:** make schema initialization constant-time ([ea8433a](https://github.com/hunterchen7/viewr/commit/ea8433ae4082c7161e4b006beb529e88f0d02508))
* decouple cache GC from session teardown ([bb73d92](https://github.com/hunterchen7/viewr/commit/bb73d9208e8d645bdb4c081b86c04460be2e094c))
* **jpeg:** decode cache JPEGs in parallel via row-aligned restart markers ([2ed0f21](https://github.com/hunterchen7/viewr/commit/2ed0f214408e27fb97d5d1b30799f5d2000b5aab))
* **jpeg:** decode row-restart cache JPEGs in parallel ([e6ca210](https://github.com/hunterchen7/viewr/commit/e6ca21009f2f95f9a52921a335f9d5eed04e00e1))
* **jpeg:** promote fastest M5 encoder ([786a772](https://github.com/hunterchen7/viewr/commit/786a77214875a1beb5aa27d9975481df19743bcc))
* **jpeg:** schedule six decode chunks per worker ([3bc4a88](https://github.com/hunterchen7/viewr/commit/3bc4a88840812c7b4f641e5247114f7ab00e0028))
* **jpeg:** select fastest dedicated worker count ([c195b86](https://github.com/hunterchen7/viewr/commit/c195b86a60047076f91d597422f6981703a5455e))
* **jpeg:** switch cache encoding to libjpeg-turbo ([cc4394a](https://github.com/hunterchen7/viewr/commit/cc4394adf791cf7211395a92440c240ea0f35baf))
* keep rating flushes off navigation ([5a0ba76](https://github.com/hunterchen7/viewr/commit/5a0ba761f1c9100dd5eadf576c14ff3c5624ac33))
* **ratings:** batch folder startup state ([d830df3](https://github.com/hunterchen7/viewr/commit/d830df39eef1017c346010b551529d74414543c4))
* **raw:** fuse integer conversion and CFA scaling ([d2b5c6c](https://github.com/hunterchen7/viewr/commit/d2b5c6c7a5c91b301dcc2be95150c5aa14304a08))
* **raw:** shorten CFA normalization ([c4f7416](https://github.com/hunterchen7/viewr/commit/c4f7416a1595f7281ec086129123b9d1162657e9))

## [0.1.1](https://github.com/hunterchen7/viewr/compare/v0.1.0...v0.1.1) (2026-07-22)


### Bug Fixes

* atomically replace cache and sidecar files ([e6ea955](https://github.com/hunterchen7/viewr/commit/e6ea955f88657a03341e756d4aba25a08a4c9e1c))
* **ci:** handle empty release output ([d578f0e](https://github.com/hunterchen7/viewr/commit/d578f0ebf9b9b0d90acbed512f633fbdadba58a3))
* **ci:** handle empty Release Please output ([18f56bd](https://github.com/hunterchen7/viewr/commit/18f56bdd94125078a26ff129834297937918ad76))
* contain engine notification panics ([39fa6d4](https://github.com/hunterchen7/viewr/commit/39fa6d4e434df7fe8b1cffe8a087cc89a81c143c))
* handle zero-sized rotations ([ee688a7](https://github.com/hunterchen7/viewr/commit/ee688a7de7dc85e02e56b3d26b8871e4baf27314))
* harden job scheduling edge cases ([da3c20e](https://github.com/hunterchen7/viewr/commit/da3c20e8365310c96e67646f55d0bb89e2a9fddf))
* harden XMP rating handling ([184e32a](https://github.com/hunterchen7/viewr/commit/184e32a2c62743f9c9df36405c23908e1ac640e9))
* hash native cache path identity ([6f4bb53](https://github.com/hunterchen7/viewr/commit/6f4bb53e199fce8e7df81f4d5daa5b7332ec4e63))
* journal pending sidecar ratings ([65148ee](https://github.com/hunterchen7/viewr/commit/65148ee2fdfce95640f93200e0fa658dcbef83d7))
* make engine jobs lifecycle-safe ([9980f37](https://github.com/hunterchen7/viewr/commit/9980f378d4bd03663888266edaa217935cc45cec))
* make persistent warming backpressure-safe ([2e91ee1](https://github.com/hunterchen7/viewr/commit/2e91ee11f283a89e301cce929ed57ca1f30d11bb))
* make rating flushes durable ([9c9523d](https://github.com/hunterchen7/viewr/commit/9c9523dc49ea6c90dd6fa7d0d2aa2d01b7977663))
* preserve completed cache work ([4f8b251](https://github.com/hunterchen7/viewr/commit/4f8b2513fafbd99fdcd084dcd36383bb9e85c9ba))
* preserve explicit zero ratings ([230d3c5](https://github.com/hunterchen7/viewr/commit/230d3c5db86a639ed4d210b40d8dd5019cc7c20e))
* recover from corrupt render cache ([954bf8e](https://github.com/hunterchen7/viewr/commit/954bf8ec41fa9ef8db9a47713a533181bac74e31))
* reject XMP without a rating subject ([c0556c4](https://github.com/hunterchen7/viewr/commit/c0556c476c8c65be7da78b5cfc2c066a83c28561))
* retain metadata when thumbnails fail ([f5cc7ef](https://github.com/hunterchen7/viewr/commit/f5cc7efcb7b067fb76939ae84161f9ab1eda2274))
* serialize and harden disk cache gc ([6ca46ae](https://github.com/hunterchen7/viewr/commit/6ca46ae9eddc877f69704d81655e9276a8c107ee))
* sync durable rename directory ([0f76432](https://github.com/hunterchen7/viewr/commit/0f76432ea2fd9529b320b1f94d59298d69145034))
* validate XMP rating context ([fdc04ae](https://github.com/hunterchen7/viewr/commit/fdc04ae15e7737d7ca963150a7c9b8a68e95dd9c))


### Performance Improvements

* add cache and orientation scaling benchmarks ([ef9bf59](https://github.com/hunterchen7/viewr/commit/ef9bf590e839951a9941167299cd13a4f36f7fd2))
* add statistical core benchmarks ([e67d3ce](https://github.com/hunterchen7/viewr/commit/e67d3ced8168aa3fc1056df2ebe5fa0d84b6818b))
* avoid repeated navigation scans ([87ac086](https://github.com/hunterchen7/viewr/commit/87ac08693867da3d1d43de54f4f4c08cf959ade9))
* bound thumbnail texture residency ([6c5a3f0](https://github.com/hunterchen7/viewr/commit/6c5a3f070aeb3b8616bef062a56886c0c6c2ae33))
* decode thumbnails only on viewport demand ([3664f6f](https://github.com/hunterchen7/viewr/commit/3664f6fa7721414c1af6a155b24ada4598793707))
* deepen testing, benchmarking, and RAW pipeline performance ([9f25831](https://github.com/hunterchen7/viewr/commit/9f25831a144591a1b0f98b35e30d36b5b74d125c))
* keep thumbnail demand viewport-current ([e803702](https://github.com/hunterchen7/viewr/commit/e803702078c5c2edb3270e638463de7eb04408f2))
* make cache eviction constant time ([8846b0b](https://github.com/hunterchen7/viewr/commit/8846b0becb70b1928550f199374fef708f0a3466))
* move cache probes off navigation ([cbbeba1](https://github.com/hunterchen7/viewr/commit/cbbeba127b26983aa635658c7f9395437a371d45))
* persist folder warm scheduling ([85575e8](https://github.com/hunterchen7/viewr/commit/85575e89c0e2d6978b89cfcf0db4ebd6bd613c99))
* persist jpeg cache off develop workers ([a357856](https://github.com/hunterchen7/viewr/commit/a3578568ef3b236d085cec88e122cdd30acc539f))
* reduce rating navigation overhead ([e2eee86](https://github.com/hunterchen7/viewr/commit/e2eee869cee25bd8ae0fed3577761585911f074d))
* resolve XMP namespaces on demand ([2b86589](https://github.com/hunterchen7/viewr/commit/2b8658908205288597331dc45f5190b290436e06))
* share entries and pin visible neighbors ([8540548](https://github.com/hunterchen7/viewr/commit/854054844ddc12fea8850bf1dd2b0c38e7771f53))
* skip full work while fitted ([50b60d6](https://github.com/hunterchen7/viewr/commit/50b60d6f3ba39a479c862aee411e91ec4b704532))
* splice validated XMP rating attributes ([1ea1310](https://github.com/hunterchen7/viewr/commit/1ea1310f42d55e90e045faa14122b1d25d029adc))
* streamline raw development ([ad88709](https://github.com/hunterchen7/viewr/commit/ad8870961f903b36ae2783f9afc15ea1da7caa82))
* tile large image rotations ([a03066e](https://github.com/hunterchen7/viewr/commit/a03066e3a901a72275f68d4ef3e75b6c817d17b9))
* virtualize the loupe filmstrip ([185ea7c](https://github.com/hunterchen7/viewr/commit/185ea7c5418294e00cbe759b524838ad922ef6df))

## [0.1.0](https://github.com/hunterchen7/viewr/compare/v0.0.1...v0.1.0) (2026-07-20)


### Features

* Linux CI and release binaries (x86_64 tarball) ([19cc5a8](https://github.com/hunterchen7/viewr/commit/19cc5a8ba70a4290d3ee37b03de16c9685dedf96))
* MIT license, Windows CI, release-please + binary releases ([fbe3032](https://github.com/hunterchen7/viewr/commit/fbe3032669897cb444ece0400ef8139997b11ce8))
