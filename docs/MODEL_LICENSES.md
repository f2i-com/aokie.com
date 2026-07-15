# Voice model notices

Aokie's automatic model installer downloads only the immutable revisions and
files listed in `models-manifest.json`. The application verifies the declared
file size and SHA-256 digest before it makes a model bundle available.

## Parakeet Unified EN 0.6B ONNX

- Source: `eschmidbauer/parakeet-unified-en-0.6b-onnx`, revision
  `7a16ff98b72e5b0beba6f182a5357c704d977b15`
- License: NVIDIA Open Model License (as declared by the model repository)
- Upstream model: NVIDIA Parakeet Unified EN 0.6B

## Pocket TTS ONNX

- Source: `KevinAHM/pocket-tts-onnx`, revision
  `58a6d00cf13d239b6748cb0769f35c580a8f606c`
- Model license: Creative Commons Attribution 4.0
- Export code license: Apache 2.0
- Upstream model: Kyutai Pocket TTS

The bundled default reference sample is the upstream repository's
`reference_sample.wav`. Aokie does not automatically install the separate
gated voice-cloning preset catalog.
