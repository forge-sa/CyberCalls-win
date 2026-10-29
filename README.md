# CyberCalls

Live call subtitles in the style of *Cyberpunk: Edgerunners*, popping up right on your Windows desktop.

While you're on an AyuGram call, everything said by you and the other person is transcribed locally with
[Whisper](https://github.com/ggml-org/whisper.cpp) and appears as a stack of animated **CALL** boxes with
typewriter text: your lines in green on the right, theirs in red on the left. No cloud, nothing leaves your machine.

- Speakers are told apart by audio stream, not guessed: AyuGram's playback is "them", the microphone it records
  from is "you".
- English and Russian, detected per phrase, so mixing languages mid-call works.
- A phrase shows up shortly after it's finished (0.8 s of silence ends it); long sentences show partial text while
  they're still being spoken.
- The overlay is click-through, never takes focus, and only exists while a call is live.

## Requirements

- Windows 10 version 2004 or newer, or Windows 11
- **AyuGram** (other Telegram Desktop forks may work, see [Configuration](#configuration))
- To build: Rust, Visual Studio Build Tools (C++), CMake and LLVM. The Vulkan SDK as well for GPU transcription.

## Setup

Install the toolchain once (accept the UAC prompt for the Build Tools):

```powershell
winget install Rustlang.Rustup Kitware.CMake LLVM.LLVM
winget install Microsoft.VisualStudio.2022.BuildTools --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
winget install KhronosGroup.VulkanSDK   # only for the GPU build
```

Open a new terminal so the new tools are on `PATH`, then:

```powershell
cd CyberCalls
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"   # or set it permanently in System Properties

# Whisper models aren't in git; get the default one (~180 MB)
mkdir models
curl.exe -L -o models\ggml-small-q5_1.bin https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small-q5_1.bin

cargo build --release                         # GPU (Vulkan), needs the Vulkan SDK
cargo build --release --no-default-features   # CPU only
```

Always build with `--release`. The first build compiles whisper.cpp and takes a few minutes.

## Usage

```powershell
.\target\release\cybercalls.exe          # leave it running, then call someone
.\target\release\cybercalls.exe --demo   # ~30 s scripted fake call, to preview the look
```

Startup takes a few seconds while Whisper loads and warms up. After that it idles until AyuGram has a call window
open **and** is recording from a microphone. The other person's name is taken from the call window's title.
Stop it with Ctrl+C.

The exe finds the model in the project's `models` folder, so it can be started from anywhere as long as the project
stays where it was built. To see each phrase, the detected language, and how long transcription took:

```powershell
$env:RUST_LOG = "cybercalls=debug"; .\target\release\cybercalls.exe
```

## Configuration

Optional. Create `%APPDATA%\cybercalls\config.toml`; anything you leave out keeps its default.

```toml
my_name = "frantix"              # shown above your own bubbles
model = 'C:\path\to\ggml-small-q5_1.bin'   # default: models\ in the project folder
use_gpu = true                   # run Whisper on the GPU (GPU build only)
threads = 8                      # CPU threads for Whisper
languages = ["en", "ru"]         # each phrase is matched to one of these
app_exe = "AyuGram.exe"          # executable of the Telegram client

[overlay]
center_x = 0.5        # horizontal center of the bubble column, fraction of screen width
top = 0.10            # where the column starts, fraction of screen height
bottom = 0.92         # oldest lines are retired once the stack reaches this
column_width = 900.0  # px
box_size = 112.0      # side of the CALL box in px; text scales with it
linger_secs = 14.0    # how long a finished line stays before fading
chars_per_sec = 55.0  # typewriter speed
```

For another Telegram client, set `app_exe` to its executable name (Task Manager → Details). Only AyuGram has been
tested.

### Choosing a model

Measured on an i7-13620H, per spoken phrase:

| Model | Size | Languages | GPU (Intel UHD, Vulkan) | CPU | Notes |
|---|---|---|---|---|---|
| `ggml-small-q5_1.bin` | 182 MB | multilingual | 0.15–0.55 s | 0.5–1.5 s (en), ~2.5 s (en+ru) | **default**; good English, readable Russian |
| `ggml-medium-q5_0.bin` | 515 MB | multilingual | 0.6–2 s | | noticeably better Russian, can lag on long phrases |
| `ggml-small.en-q5_1.bin` | 182 MB | English only | 0.1–0.5 s | | set `languages = ["en"]` |
| `ggml-large-v3-turbo-q5_0.bin` | 548 MB | multilingual | 40 s+ | | too slow on integrated graphics |

The GPU numbers were measured on the same laptop under Linux and haven't been re-measured on Windows yet.
With two languages, every phrase runs Whisper's encoder twice (once to detect the language), which hurts most on the
CPU. If you only need English, `languages = ["en"]` roughly halves the time.

All models are at `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/<file>`.

## How it works

```
window poll ── AyuGram call window opens/closes, its title = peer name ─┐
                                                                        ├─► coordinator ─► Whisper ─► overlay
WASAPI ── AyuGram's mic session (you) + loopback of its playback (peer) ┘     (VAD)        (en/ru)    (DX12)
```

1. **Call detection** (`src/window.rs`, `src/audio.rs`): a call is live while AyuGram has a non-main window open
   *and* an active capture session on some microphone. Requiring both keeps voice-message recording from
   triggering it.
2. **Audio** (`src/audio.rs`): the microphone that holds AyuGram's session is captured as you; AyuGram's own playback
   is captured with per-process loopback as the peer. Windows converts both to 16 kHz mono.
3. **Segmentation** (`src/vad.rs`): an energy-based voice detector with an adaptive noise floor cuts each feed into
   phrases, and emits snapshots every 1.5 s for live partial text.
4. **Transcription** (`src/stt.rs`): whisper.cpp via `whisper-rs`. Three things keep it fast and clean:
   - the encoder window is shrunk from Whisper's fixed 30 s to the phrase's length (about 20× faster);
   - the language is picked between the configured ones only, with a bias toward what that person spoke last;
   - repetition loops and Whisper's well-known phantom lines ("Thanks for watching", "Субтитры сделал…") are filtered.
   Finished phrases are always processed before partial ones.
5. **Overlay** (`src/overlay.rs`, `src/overlay/window.rs`, `src/callbox.wgsl`): a topmost, click-through window over
   the primary monitor, drawn with wgpu on DX12 through a DirectComposition swapchain for per-pixel transparency. The
   CALL box (scanlines, grid, corner brackets, spinning two-arc ring, glitch/RGB-split pop-in) is a single shader;
   text is rendered with glyphon in the Tektur font. The ring spins faster while that person is talking.

`.cargo/config.toml` names the CMake generator. Without it, cmake-rs strips every `/O` flag for MSVC and whisper.cpp
is built unoptimized (about 10× slower). If you build with another Visual Studio version, change the generator name
there and run `cargo clean -p whisper-rs-sys --release`.

## Benchmarking transcription

`examples/bench.rs` runs the segmenter and Whisper over a 16 kHz mono 16-bit WAV and prints each phrase with its
timing:

```powershell
ffmpeg -f dshow -i audio="<mic name>" -ac 1 -ar 16000 test.wav   # or any recorder; q to stop
cargo run --release --example bench -- models\ggml-small-q5_1.bin test.wav        # GPU
cargo run --release --example bench -- models\ggml-small-q5_1.bin test.wav cpu    # CPU
```

Environment knobs: `LANGS=en,ru`, `THREADS=8`, `FULL_CTX=1` (disable the shrunken encoder window), `CTX_PAD`,
`CTX_MIN`.

## Troubleshooting

- **Nothing appears during a call.** Run with `RUST_LOG=cybercalls=debug`. If you see `call audio up` but no
  `call window candidate`, the call window wasn't recognized; if you see neither, check `app_exe`.
- **Your mic shows the other person's words too.** Your mic is hearing your speakers. Use headphones.
- **Transcription is very slow.** Make sure `.cargo/config.toml` matches your Visual Studio version, then rebuild
  with `cargo clean -p whisper-rs-sys --release`. With two languages on CPU, consider the GPU build.
- **The build fails with "An Application Control policy has blocked this file".** Smart App Control blocks the
  unsigned programs Cargo builds. It has to be turned off (Windows Security → App & browser control) to build.
- **The build can't find libclang.** Set `LIBCLANG_PATH` to `C:\Program Files\LLVM\bin`.
- **Russian comes out as English gibberish.** You're probably on an English-only model (`.en` in the file name).
  With `ru` in `languages`, the app exits at startup with an error saying so.

## Known limitations

- Not yet tested on a real call on Windows.
- The overlay covers the primary monitor only.
- The peer feed includes every sound AyuGram plays (notification sounds too), not just the call.
- Group voice chats are untested. If they work, everyone except you will show up under the chat's name.
- If a separate AyuGram chat window is open while you record a voice message, it can be mistaken for a call.
- Sizes are in physical pixels, so the overlay looks smaller on high-DPI screens.

## Credits

- Speech recognition: [whisper.cpp](https://github.com/ggml-org/whisper.cpp) through [whisper-rs](https://codeberg.org/tazz4843/whisper-rs).
- Font: [Tektur](https://github.com/hyvyys/Tektur), SIL Open Font License (`assets/fonts/OFL.txt`).
- Visual style inspired by *Cyberpunk: Edgerunners*; no assets from the show are included.
