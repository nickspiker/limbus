<p align="center">
  <img src="https://raw.githubusercontent.com/nickspiker/iris/main/iris.webp" alt="iris — the aperture the light passes through" width="512">
</p>

# iris

Sensor-intake adapter for the VERICHROME imaging stack: foreign image formats in, sensor truth out — [`RawInfo`] (dimensions, black/white levels, CFA tile, both DNG ColorMatrices with their CalibrationIlluminant codes) plus the sensor counts at native bit depth, exactly as the camera recorded them.

Bidirectional by intent: today iris reads (DNG and, via `rawler`'s decoders, most camera RAW formats); the DNG/TIFF writers migrate in next, so every foreign format crosses the stack boundary in one place.

## What it feeds

- [**opsin**](https://github.com/nickspiker/opsin) — the spectral viewer/converter — translaterates iris's output into [VSF-Image](https://github.com/nickspiker/vsf): sensor counts stored untouched, characterization and view intent as metadata.
- **chameleon** — the VERICHROME calibration core — reads target scans through the same intake.

## The pipeline, end to end

```console
$ opsin --convert shot.dng     # iris decodes, opsin writes VSF-Image
opsin: wrote shot.vsf
$ vsfinfo shot.vsf tree        # inspect what was preserved
├── spectral_image  (dims, CFA, black/white, 14-bit sensor counts)
└── colour_profile  (camera→VSF-RGB, both DNG matrices verbatim, illuminants)
```

## Design rules

- **No colour interpretation.** iris hands over matrices and counts; deciding what they mean is the caller's job. `rawler` is used strictly as a decompression black box — none of its types or colour handling crosses into the pipeline.
- **Metadata by hand.** A hand-rolled TIFF/IFD parser reads the tags, so exactly which `ColorMatrix1`/`ColorMatrix2`, black, white, and CFA values flow downstream is under explicit control.
- **Native depth, always.** 14-bit counts stay 14-bit. No promotion, no rescale, no "convenience" normalization.

## License

MIT OR Apache-2.0
