# Audio quality and client identity

## What the display means

- **MAX** is a catalogue tag indicating a hi-res lossless version exists.
- **HI-FI** is the catalogue's standard lossless tag.
- The now-playing codec, bit depth and sample rate describe the selected stream,
  not the best version in the catalogue. Native FLAC playback reads STREAMINFO
  directly and uses its source bit depth and sample rate to verify Tidal's
  playback metadata. A discrepancy is logged.
- **Symphonia** names the native decoder library; **FLAC** names the codec.
- AAC has no source PCM bit depth comparable to FLAC. Sample rate and encoded
  bitrate are useful AAC measurements; a decoder's output depth is not.

Lossless compression preserves the PCM supplied to the encoder, but does not
prove anything about an earlier lossy source or the quality of the master. These
measurements do not establish that different streams use identical masters or
that a higher-resolution stream sounds better.

## Verified delivery

Read-only probes requested `HI_RES_LOSSLESS` and independently checked downloaded
headers with ffprobe. The same track IDs were compared across client sessions.
No user tokens or personal library lists are included here.

| Client identity/session | Sample | Delivery confirmed from audio headers |
|---|---|---|
| Built-in Automotive identity | Six MAX and six HI-FI favourites | All FLAC, 16-bit / 44.1 kHz |
| Cliamp's published TV/device identity | The same 12 favourites | Six MAX tracks: 24-bit FLAC, five at 44.1 kHz and one at 48 kHz; six HI-FI tracks: AAC, approximately 320 kbps / 44.1 kHz |
| Existing TIDAL Hi-Fi web-wrapper session | Three MAX and three HI-FI tracks from that sample | MAX: 24-bit / 44.1 kHz FLAC; HI-FI: 16-bit / 44.1 kHz FLAC; explicit LOSSLESS requests also returned 16-bit FLAC |

The tested hi-res TV streams and both quality tiers from the web session used
**unencrypted v1 MPEG-DASH**. That is distinct from the DRM-protected v2 manifests
previously tested. The web wrapper's use of Widevine does not imply that every
stream requested with its session must be encrypted.

Client-specific backend rules appear to govern delivery. The probes establish
behaviour for this sample and these sessions, not the undocumented reason for
those rules or a guarantee for every track, region or future session.

## What Riptide supports now

Riptide requests hi-res first, standard lossless second and HIGH AAC last. It
accepts only formats supported by the selected backend:

- **Native (default):** direct FLAC and AAC/MP4 resources. Segmented FLAC and
  DASH are skipped; the stream resolver tries the next supported tier.
- **mpv:** also handles DASH and segmented FLAC through the localhost manifest
  server. mpv's decoder output format must not be used as a source-bit-depth
  measurement.

The default credentials still returned standard lossless in the live sample.
Requesting hi-res does not itself change the client identity's permissions.

Native DASH playback and sustainable web-client authorization/renewal remain
future work. An access token borrowed from a desktop session is temporary, not a
complete authentication implementation. No identifiable refresh-token field was
found in the inspected web wrapper's IndexedDB, local storage or cookie metadata;
its renewal mechanism has not been established. Sharing a rotating refresh token
between applications could interfere with their sessions.

Developer-portal registration does not establish full-track hi-res entitlement.
The authorization guide describes obtaining tokens; the SDK overview describes
third-party playback as previews. Portal clients also do not support Riptide's
current device authorization flow.

## References

- [Tidal authorization](https://developer.tidal.com/documentation/api-sdk/api-sdk-authorization)
- [Tidal API/SDK overview](https://developer.tidal.com/documentation/api-sdk/api-sdk-overview)
- [Cliamp Tidal integration](https://github.com/bjarneo/cliamp/blob/main/docs/tidal.md)
- [Cliamp quality selection](https://github.com/bjarneo/cliamp/blob/main/external/tidal/manifest.go)
