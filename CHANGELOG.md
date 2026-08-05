# Changelog

## [0.6.0](https://github.com/hunterchen7/viewr/compare/v0.5.0...v0.6.0) (2026-08-05)


### Features

* add minimal, verbose, and none label modes to the info strip ([#38](https://github.com/hunterchen7/viewr/issues/38)) ([f2ecef6](https://github.com/hunterchen7/viewr/commit/f2ecef6ef926c51c0f3e8cb9b8874704cceb87f8))
* **app:** produce view hints and consume decode-band tiles ([e5903c1](https://github.com/hunterchen7/viewr/commit/e5903c1c031569f50bf2a3f11dd8f4aeb375607b))
* band-first JPEG rehydrate — sharp visible rows in ~17ms instead of ~43ms ([a4622fa](https://github.com/hunterchen7/viewr/commit/a4622fa06dacbeb75ba9eab4d24bb57621bbc94c))
* **core:** advisory view-hint mailbox and band publication in rehydrates ([02c5636](https://github.com/hunterchen7/viewr/commit/02c5636a6917166375b3a49633a126814b6a2cf2))
* **core:** single-slot FullBand side channel on RamCache ([254f886](https://github.com/hunterchen7/viewr/commit/254f886a972c2dedb8d581b5d5207a9e1125d6d9))
* **core:** two-phase visible-band-first restart-JPEG decode ([1a1d96f](https://github.com/hunterchen7/viewr/commit/1a1d96fe8ace392c9415ab77da087d7328aceda2))


### Bug Fixes

* archive rawler criterion reports ([43c960e](https://github.com/hunterchen7/viewr/commit/43c960eeda3f280f39db216265b07cd580f3d118))
* bound restart decode allocation geometry ([8cd0e78](https://github.com/hunterchen7/viewr/commit/8cd0e785f998e28bdc948e54b16291a5b72a8886))
* compose the overlay across buffer, staging, and band sources ([d40dec5](https://github.com/hunterchen7/viewr/commit/d40dec556873561b714ec347ec2c72849cb4b4ed))
* debounce band-tile draining across one probe race frame ([89fdc76](https://github.com/hunterchen7/viewr/commit/89fdc761c7283c992529ab624e8107cee4a214f9))
* encode partial progressive mcus correctly ([a9be080](https://github.com/hunterchen7/viewr/commit/a9be0809b032ff199782b2874c538cce2e4a4568))
* fence interactive publications to navigation ([16cc0d4](https://github.com/hunterchen7/viewr/commit/16cc0d4903d63ec358f6edf44bf64b1f5a51816f))
* harden generic raw parallel decode ([5e60f7a](https://github.com/hunterchen7/viewr/commit/5e60f7acb9f3dc4aebde579bd7804d4e6eec89c8))
* harden raw pixel access invariants ([fedcc6b](https://github.com/hunterchen7/viewr/commit/fedcc6b0d62138613a651dbc17fbd20d46fa81ae))
* harden the decode share against regeneration races ([3e632ae](https://github.com/hunterchen7/viewr/commit/3e632aea2a39b4de6bdbacde33a245f666dbc131))
* harden the progressive staging lifecycle ([c6ea267](https://github.com/hunterchen7/viewr/commit/c6ea2673cf22195a2855ff6073d40c33ede5a8e7))
* initialize jpeg decoder storage safely ([177ff12](https://github.com/hunterchen7/viewr/commit/177ff12aa314a85ac6797d4dd47ba1cab69d22c2))
* keep the portrait region test out of CI's pinned-fixture pass ([8e82ee3](https://github.com/hunterchen7/viewr/commit/8e82ee3036857226e6fba30beacdcf99dcb6d48a))
* make parallel PPG component access sound ([989d3ab](https://github.com/hunterchen7/viewr/commit/989d3abf80eebb75ce4489dd9850969ea590e3b6))
* make progressive full publication race free ([eaccfb2](https://github.com/hunterchen7/viewr/commit/eaccfb28737137f9bc578bdd310a0b7dacef27a0))
* make tiled raw partitions provenance-safe ([14713dd](https://github.com/hunterchen7/viewr/commit/14713dd7d68556e7943f5f078f16e9729cb36162))
* move the rawler fork submodule out of the vendor directory ([1450079](https://github.com/hunterchen7/viewr/commit/14500794afee4f9bf8173192310052ea8f776e43))
* never park a share lead inside the regeneration test ([54b36ba](https://github.com/hunterchen7/viewr/commit/54b36ba4c489ae6c4dbf7c3050ba2c225b318c6e))
* pin rawler benchmark report path ([871f989](https://github.com/hunterchen7/viewr/commit/871f989874a3d64744fb8986b95893d033865339))
* prove demosaic allocation geometry ([bb6921e](https://github.com/hunterchen7/viewr/commit/bb6921e73e85d3e8c1050fcd39da9b482c2e0fbe))
* reject truncated entropy marker reads safely ([fbe5918](https://github.com/hunterchen7/viewr/commit/fbe59187c0be33cb14ad42946e6be099a85aed9b))
* repair release plumbing for the in-tree rawler fork ([1576a59](https://github.com/hunterchen7/viewr/commit/1576a5987acf3dd56d4014c21bd7a65c50fd12d7))
* require an early band only when the request stays partial ([e957aab](https://github.com/hunterchen7/viewr/commit/e957aab389e4cae3479faed02c99f8ad1b4775bc))
* resolve the bake-off workspace's rawler from the in-tree fork ([2a428a5](https://github.com/hunterchen7/viewr/commit/2a428a51655bdd102073922d6d3a5531749e9bd2))
* stage the rawler fork on both sides of cargo vendor ([04cb243](https://github.com/hunterchen7/viewr/commit/04cb24314686b45d0bdee98e74b2fd23fcae7496))
* validate bounded Sony LJPEG tiles ([4e97b3c](https://github.com/hunterchen7/viewr/commit/4e97b3c92e74191fbfaa9d04107e1c96acfc7edd))
* vendor rawler's locked release graph ([ac23045](https://github.com/hunterchen7/viewr/commit/ac2304585d2b126756b73971b9c33bb187ffdb4a))
* warm the next image before trickling off-screen tiles ([79cf6d9](https://github.com/hunterchen7/viewr/commit/79cf6d9d309b9e213d02a72c043e1607c7f4ef79))


### Performance Improvements

* accelerate progressive PPG regions ([449afa2](https://github.com/hunterchen7/viewr/commit/449afa210f28d5f0f8b2616d776873d2343ae44a))
* add structured raw pipeline records ([87eb41d](https://github.com/hunterchen7/viewr/commit/87eb41dfff0ce653350a3571c406f495efffefac))
* adopt the fused-PPG rawler fork as an in-tree submodule ([6991de9](https://github.com/hunterchen7/viewr/commit/6991de9f139dd1069dab46a8218829bd5e96f0b5))
* audit and accelerate the full image pipeline ([16f2c76](https://github.com/hunterchen7/viewr/commit/16f2c767934bd6925f2b02af5f174be2c9cb1156))
* behavior-preserving browsing/loading speed pass ([e59b0b5](https://github.com/hunterchen7/viewr/commit/e59b0b5ea15b6aa52a7388b95eace1c1ec1e09e6))
* bound background event work per frame ([306181d](https://github.com/hunterchen7/viewr/commit/306181d8877295f3597ce5e59ee37adb5f0e9bd4))
* carry opaque pixel provenance to textures ([581033e](https://github.com/hunterchen7/viewr/commit/581033e52d77989856750bc28da7bdeaf9b354d9))
* cut one frame from every keypress and sharpen viewports in one frame ([2e0b4c6](https://github.com/hunterchen7/viewr/commit/2e0b4c65446d054a9bf2d5711c31bc42bbe5da93))
* decode Sony LJPEG tiles directly ([3559795](https://github.com/hunterchen7/viewr/commit/35597950fe2a05b2961da91cc2ee38287711a871))
* faster ARW decode and warm decoder databases at startup ([ddf24d0](https://github.com/hunterchen7/viewr/commit/ddf24d0f2d9cca3bab2df9df2093d20abc02efa8))
* fuse integer browse normalization ([2cb7f2a](https://github.com/hunterchen7/viewr/commit/2cb7f2adff0d5033f4a768ef94a03687b706f529))
* group refreshed ratings off the UI thread ([9f4a8c2](https://github.com/hunterchen7/viewr/commit/9f4a8c26ab2baf19c2cacbf1434dcf9dab31ccdf))
* in-tree fused-PPG rawler via submodule ([981fee4](https://github.com/hunterchen7/viewr/commit/981fee4f5bbee2bdb392338f877585b220d136fe))
* keep preferences close off filesystem I/O ([800d413](https://github.com/hunterchen7/viewr/commit/800d413ecb2fc904892d34a6bae024140763bd21))
* make first progressive region uploadable ([415fdae](https://github.com/hunterchen7/viewr/commit/415fdaeb10b2a8816316550bb9bf87864320963e))
* move config durability off the UI thread ([db047f1](https://github.com/hunterchen7/viewr/commit/db047f1fe3c9dbd7c76c04bdbae0be878a617914))
* parallel-tile ARW decode, inlined huffman, startup DB prewarm ([f612ccd](https://github.com/hunterchen7/viewr/commit/f612ccd5809ed7598bdf6df4a87f3ef4aac23d8e))
* parallelize restart-row jpeg encoding ([eee7d9b](https://github.com/hunterchen7/viewr/commit/eee7d9b61b22050a9284e964436845a12b380bd0))
* reuse Sony tile decode tables ([7af2d75](https://github.com/hunterchen7/viewr/commit/7af2d75ce0903f34d6f4e6d3963f3e2258fb57ac))
* same-frame keypress navigation and one-frame viewport sharpening ([5baa571](https://github.com/hunterchen7/viewr/commit/5baa571e5cb1b402501a1740ab92114a9864d3d6))
* shorten progressive texture publication locks ([1de8f4f](https://github.com/hunterchen7/viewr/commit/1de8f4f1a46ef4342aebc468cb0ce9748714c57c))

## [0.5.0](https://github.com/hunterchen7/viewr/compare/v0.4.0...v0.5.0) (2026-08-02)


### Features

* update macOS app directly ([4508c6e](https://github.com/hunterchen7/viewr/commit/4508c6e8df78e8c4fc80a4f2621d7696a393f0e2))
* update the installed app directly ([f5c0e6a](https://github.com/hunterchen7/viewr/commit/f5c0e6a4cc022e39040a9658c1a86325ca260ceb))


### Bug Fixes

* close updater recovery races ([88e980a](https://github.com/hunterchen7/viewr/commit/88e980a27b4aa6b4432bd4d22a23ef7b8249c546))
* consume direct-update paths on other platforms ([d72600a](https://github.com/hunterchen7/viewr/commit/d72600ab9579760175f0f51c06dc6a9f22a8ed4d))
* harden direct update transaction ([52d09a4](https://github.com/hunterchen7/viewr/commit/52d09a4ae7314f90ad956fcbec6c783c15079e07))
* harden update note layout ([c43d74e](https://github.com/hunterchen7/viewr/commit/c43d74e62e9efea5e81dfdfeb766ce2aed80a975))
* hold updater gate until process exit ([dd61a6c](https://github.com/hunterchen7/viewr/commit/dd61a6c5e1e37a428456f8527bf273595af57a77))
* make macOS updates recoverable ([af7c78f](https://github.com/hunterchen7/viewr/commit/af7c78fc37c51284e71cfb47d5e0810c6e835fd7))
* pin macOS package install location ([dc4299c](https://github.com/hunterchen7/viewr/commit/dc4299c37c1ece032b96758dbc3fdbbf748805ad))
* preserve macOS package recovery lifecycle ([0e31ebc](https://github.com/hunterchen7/viewr/commit/0e31ebc1d3a8ff652402615deafcc53dc3085a90))
* render update notes as readable markdown ([6e04161](https://github.com/hunterchen7/viewr/commit/6e0416137119b624cf48b569358069ddd981e4c1))
* retain recovery until package install completes ([c598a83](https://github.com/hunterchen7/viewr/commit/c598a835543456faca3a60de99478e530609dad0))
* wait for updater parent before recovery ([f70bb60](https://github.com/hunterchen7/viewr/commit/f70bb60e46361cf3347fa81d9c6713661307b258))

## [0.4.0](https://github.com/hunterchen7/viewr/compare/v0.3.0...v0.4.0) (2026-08-02)


### Features

* add a configurable image information strip with selectable fields and top or bottom placement ([#24](https://github.com/hunterchen7/viewr/pull/24))
* fill the Full-resolution RAM cache adaptively with byte-budgeted, direction-biased prefetch beyond adjacent images ([#26](https://github.com/hunterchen7/viewr/pull/26))


### Bug Fixes

* harden adaptive prefetch scheduling ([2cff689](https://github.com/hunterchen7/viewr/commit/2cff689b4ebf97bd4a94b8ace98cdf1d69eec04c))
* keep image info overflow draggable ([650b8f6](https://github.com/hunterchen7/viewr/commit/650b8f67e0a7c20a709ef20bbf051ce3425cd115))


### Performance Improvements

* release evicted cache owners after unlock ([cfac26d](https://github.com/hunterchen7/viewr/commit/cfac26da114c14a689a61062654184534d20c26d))

## [0.3.0](https://github.com/hunterchen7/viewr/compare/v0.1.1...v0.3.0) (2026-07-29)

This is the first public release after v0.1.1. The v0.2.0 draft was not
published, so these notes cover the complete public upgrade.

### Highlights since v0.1.1

* Native macOS, Windows, and Debian installers, ARW/MIME integration, portable archives, and a vendored source archive.
* Verified in-app updates with release notes and Download now, Later, and Skip this version choices.
* Restored window size and position plus configurable loading text, image information, and loading indicators.
* Full-resolution preloading for the current and adjacent images, with viewport-first tiled loading while zoomed.
* Selectable JPEG cache quality, faster jpeg-rusturbo encoding, parallel restart-marker decoding, and better dark-gradient preservation.
* Brighter, highlight-safe RAW development and extensive cache, decode, rating, XMP, and sidecar-persistence hardening.

### Breaking changes carried forward from the unpublished draft

* **db:** `record_rating_pending_sidecar` now rejects paths without a resolvable physical sidecar owner instead of journaling unresolved work. `Db::open` also requires WAL-capable storage, and `DbError` adds `WalUnavailable`.

### Changes added after the v0.2.0 draft

#### Features

* add clickable star markers to the filmstrip scrollbar ([a14d411](https://github.com/hunterchen7/viewr/commit/a14d411a2bcb2dab5c9a599c139ef64e2e92b7a2))
* add configurable image-processing thread limits ([805a6b8](https://github.com/hunterchen7/viewr/commit/805a6b88da8c96edce985206ad09cf0316575c5f))
* add optional vertical filmstrip scrolling ([0295134](https://github.com/hunterchen7/viewr/commit/0295134301a7aac542b7c615b79675b48fc4c3cb))

#### Bug fixes

* **ci:** harden release recovery and publication ([448c2c7](https://github.com/hunterchen7/viewr/commit/448c2c79333b061675e778351832808da53921c2))
* **ci:** prefetch the complete license graph ([#22](https://github.com/hunterchen7/viewr/issues/22)) ([cd59c8f](https://github.com/hunterchen7/viewr/commit/cd59c8f2cf3adf9b1f803f27480a01bd5c47e587))
* keep preferences controls reachable ([ca9152c](https://github.com/hunterchen7/viewr/commit/ca9152c2b1bb7792eb225433e06790e5e97e038f))
* **packaging:** ship required JPEG attribution ([c846ac4](https://github.com/hunterchen7/viewr/commit/c846ac4d9448630e4dade1f4ba98899efbbedb44))

### Changes carried forward from the unpublished v0.2.0 draft

#### Features

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

#### Bug fixes

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

#### Performance improvements

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
