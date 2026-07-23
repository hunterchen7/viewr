# Changelog

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
