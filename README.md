<p align="center">
  <img src="https://raw.githubusercontent.com/nickspiker/iris/main/iris.webp" alt="iris — the aperture the light passes through" width="512">
</p>

# iris

Any image format in. [VSF-Image](https://github.com/nickspiker/vsf) out.

iris is the format gateway of the VERICHROME imaging stack: camera RAW, DNG, TIFF, JPEG, PNG — creative formats or sensor-truth ones, eventually even PSD — cross the stack boundary here, in one place, and come out the other side as VSF-Image: the pixel data stored untouched at native depth, and *what those pixels mean* carried as tiered characterization metadata instead of being baked in.

The honesty is in the tier. Every source declares what it actually knows:

| Source | What iris preserves | Profile grade |
|---|---|---|
| Target-scanned camera | magic-9 solve + patches + calibration provenance | `unit` — measured on THIS camera |
| Camera RAW / DNG | native-depth sensor counts, CFA, black/white, both ColorMatrices + illuminants, verbatim | `model` — factory characterization |
| JPEG / PNG / TIFF / PSD | decoded code values + the transfer they arrived in | `assumed` — the format convention implies the observer |
| Anything else | the pixels, honestly unlabeled | none — uncharacterized, never a fake guess |

Downstream, [**opsin**](https://github.com/nickspiker/opsin) translaterates and views, **chameleon** calibrates — neither ever touches a foreign format directly.

## Today / landing

**Wired now:** camera RAW + DNG decode (hand-rolled TIFF/IFD metadata parse; `rawler` strictly as a decompression black box) → dimensions, CFA tile, black/white levels, both DNG ColorMatrices with CalibrationIlluminant codes, and sensor counts at native bit depth. The VSF-Image write currently lives in opsin's convert path.

**Landing:** the VSF-Image write moves in (format in → `.vsf` out becomes one call), assumed-observer ingest (JPEG/PNG/TIFF/PSD), and the DNG/TIFF writers migrate over from chameleon — the aperture passes light both directions.

## The pipeline, end to end

```console
$ opsin --convert shot.dng     # iris decodes, opsin writes VSF-Image
opsin: wrote shot.vsf
$ vsfinfo shot.vsf tree        # inspect what was preserved
├── spectral_image  (dims, CFA, black/white, 14-bit sensor counts)
└── colour_profile  (camera→VSF-RGB, both DNG matrices verbatim, illuminants)
```

## Design rules

- **No colour interpretation.** iris hands over matrices, transfers, and counts; deciding what they mean is characterization's job, downstream. Decoders are decompression black boxes — none of their types or colour handling crosses into the pipeline.
- **Metadata by hand.** A hand-rolled TIFF/IFD parser reads the tags, so exactly which values flow downstream is under explicit control.
- **Native depth, always.** 14-bit counts stay 14-bit, 8-bit code values stay 8-bit. No promotion, no rescale, no "convenience" normalization.
- **Never guess silently.** A source with no implied observer gets no profile — uncharacterized is a first-class, honest state.

## License

MIT OR Apache-2.0
