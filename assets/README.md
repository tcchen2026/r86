# Guest tools

One build produces two local ISO 9660/Joliet data CDs:

- `r86_tools.iso`: public contents, r86-tool and the unchanged MIT VMDisp9x
  2D release. Roots are in `r86_tools.json`.
- `r86_tools_full.iso`: internal only, the public contents plus the original
  VBEMP and user-supplied Super PI packages from `r86_tools_full.json`.
  **Never publish or distribute the full ISO.** VBEMP's license restricts
  bundling; Super PI also stays internal. Preserve their original licenses.

Source READMEs and checksums in each asset directory record provenance.
Guest binaries, source assets and generated images stay outside Git. Both
images have separate README.TXT and SHA256 files. The standalone local VBEMP
ISO is retained for driver tests. No existing internal tool is discarded.

Rebuild after preparing every source directory:

```sh
python3 -m pip install --target tmp/iso-tools pycdlib
PYTHONPATH=tmp/iso-tools python3 script/build_tools_iso.py
```

The build fails on a missing source instead of silently removing a tool.
Add public tools to the public manifest; add private extras only to the full
manifest. r86-tool finds a user-provided VBEMP package on a CD or accepts its
extracted directory; the public ISO does not include VBEMP.
