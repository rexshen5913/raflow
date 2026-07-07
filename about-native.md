## Embedded native components (not Rust crates)

The following are compiled into / embedded in the raflow binary but are not
Rust crates, so they are documented here manually.

### whisper.cpp (vendored via whisper-rs-sys) — MIT

The GGML / whisper.cpp C/C++ inference engine is statically compiled into the
raflow binary (Metal backend). Its full license text:

```
MIT License

Copyright (c) 2023-2024 The ggml authors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

### OpenCC conversion dictionaries (embedded via ferrous-opencc) — Apache-2.0

raflow embeds the Simplified→Traditional Chinese conversion dictionaries that
originate from the OpenCC project (Open Chinese Convert, https://github.com/BYVoid/OpenCC),
Copyright (c) 2010-2024 Carbo Kuo (BYVoid) and OpenCC contributors, licensed under
Apache-2.0 (full text reproduced in the Apache-2.0 section above).

### Downloaded model weights

The Whisper and Silero VAD model weights are downloaded at install time (not
shipped inside the app). They are credited in the project README:
OpenAI Whisper (MIT), whisper.cpp GGML conversions by Georgi Gerganov (MIT),
and Silero VAD (MIT).
