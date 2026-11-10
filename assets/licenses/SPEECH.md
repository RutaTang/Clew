# Speech — the libraries `clew-speak` is built with, and the voices it speaks in

`clew-speak` is the program beside Clew that reads its answers aloud. It is
built with the prebuilt libraries of **sherpa-onnx** 2.0.1
(<https://github.com/k2-fsa/sherpa-onnx>, the release's `static-lib` archive
on Linux and macOS, its `shared-MT-Release-lib` DLLs on Windows), which hold
these libraries, each unmodified:

| Library | License | Copyright |
| --- | --- | --- |
| sherpa-onnx (<https://github.com/k2-fsa/sherpa-onnx>) | Apache-2.0 | Xiaomi Corporation and the sherpa-onnx authors |
| ONNX Runtime (<https://github.com/microsoft/onnxruntime>) | MIT | Copyright (c) Microsoft Corporation |
| kaldi-native-fbank (<https://github.com/csukuangfj/kaldi-native-fbank>) | Apache-2.0 | Xiaomi Corporation and its authors |
| kaldi-decoder (<https://github.com/k2-fsa/kaldi-decoder>) | Apache-2.0 | Xiaomi Corporation and its authors |
| kaldifst (<https://github.com/k2-fsa/kaldifst>), with OpenFst (<https://www.openfst.org>) | Apache-2.0 | Xiaomi Corporation; Google, Inc. and the OpenFst authors |
| simple-sentencepiece (<https://github.com/pkufool/simple-sentencepiece>) | Apache-2.0 | its authors |
| cppjieba (<https://github.com/yanyiwu/cppjieba>) | MIT | Copyright (c) 2013 Yanyi Wu |
| utf8proc (<https://github.com/JuliaStrings/utf8proc>) | MIT | Copyright (c) 2014-2021 Steven G. Johnson, Jiahao Chen, Tony Kelman, Jonas Fonseca, and other contributors; Copyright (c) 2009 Public Software Group e. V., Berlin, Germany |
| KISS FFT (<https://github.com/mborgerding/kissfft>) | BSD-3-Clause | Copyright (c) 2003-2010, Mark Borgerding |

The Apache License 2.0 is in [`Apache-2.0.txt`](Apache-2.0.txt), beside
this file; the MIT and BSD-3-Clause licenses follow.

## Echo cancellation

So that the reader can talk over Clew while it speaks, Clew takes its own
voice back out of what the microphone hears with **sonora**
(<https://github.com/dignifiedquire/sonora>), a Rust port of the WebRTC audio
processing module's echo canceller (AEC3), built into Clew itself:

| Library | License | Copyright |
| --- | --- | --- |
| sonora (<https://github.com/dignifiedquire/sonora>), from WebRTC (<https://webrtc.org>) | BSD-3-Clause | Copyright (c) 2011, The WebRTC Project Authors; Copyright (c) 2016, Arun Raghavan and contributors; Copyright (c) 2026, dignifiedquire. All rights reserved. |

## Waiting for "Hey, Clew"

So that "Hey, Clew" starts listening, `clew-speak` runs sherpa-onnx's
keyword spotter (above). Its model is not shipped with Clew: it is
downloaded only when the user asks for it, in Settings → Voice, from the
sherpa-onnx project's release, checked against its published size and
SHA-256; the four files it runs from are taken out of it, each checked too.

- **sherpa-onnx-kws-zipformer-zh-en-3M-2025-12-20**, by the k2-fsa project
  (<https://github.com/k2-fsa/sherpa-onnx/releases/tag/kws-models>), under
  the Apache License 2.0.

The archive it comes in is read by these libraries, built into Clew itself:

| Library | License | Copyright |
| --- | --- | --- |
| tar (<https://github.com/alexcrichton/tar-rs>) | MIT OR Apache-2.0 | Alex Crichton and the tar-rs contributors |
| bzip2 (<https://github.com/trifectatechfoundation/bzip2-rs>) | MIT OR Apache-2.0 | Alex Crichton and the bzip2-rs contributors |
| libbz2-rs-sys (<https://github.com/trifectatechfoundation/libbzip2-rs>), a Rust translation of bzip2's libbzip2 | bzip2-1.0.6 (below) | Copyright (C) 1996-2021 Julian R Seward; Copyright (C) 2019-2020 Federico Mena Quintero; Copyright (C) 2021 Micah Snyder; the translation Copyright (C) 2024-2025 Trifecta Tech Foundation and contributors |

## The voices

No voice ships with Clew. Each is downloaded only when the user asks for it,
in Settings → Voice, from the sherpa-onnx project's copies on Hugging Face,
at a revision Clew pins and checks file by file:

- **Supertonic 3**, by Supertone Inc. (<https://huggingface.co/Supertone/supertonic-3>),
  under the BigScience OpenRAIL-M license, whose use restrictions apply to
  anyone who uses it (<https://huggingface.co/Supertone/supertonic-3/blob/main/LICENSE>).
- **Kokoro** 1.0, by hexgrad (<https://huggingface.co/hexgrad/Kokoro-82M>),
  under the Apache License 2.0.

## MIT License

Applies to ONNX Runtime, cppjieba and utf8proc, each with its copyright
above.

> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to
> deal in the Software without restriction, including without limitation the
> rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
> sell copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in
> all copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
> FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
> IN THE SOFTWARE.

## BSD 3-Clause License

Applies to KISS FFT and to sonora, each with its copyright above.

> Redistribution and use in source and binary forms, with or without
> modification, are permitted provided that the following conditions are met:
>
> 1. Redistributions of source code must retain the above copyright notice,
>    this list of conditions and the following disclaimer.
> 2. Redistributions in binary form must reproduce the above copyright
>    notice, this list of conditions and the following disclaimer in the
>    documentation and/or other materials provided with the distribution.
> 3. Neither the name of the copyright holder nor the names of its
>    contributors may be used to endorse or promote products derived from
>    this software without specific prior written permission.
>
> THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
> AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
> IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
> ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE
> LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR
> CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF
> SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS
> INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN
> CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE)
> ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE
> POSSIBILITY OF SUCH DAMAGE.

## bzip2-1.0.6 License

Applies to libbz2-rs-sys, with its copyright above.

> Redistribution and use in source and binary forms, with or without
> modification, are permitted provided that the following conditions are
> met:
>
> 1. Redistributions of source code must retain the above copyright notice,
>    this list of conditions and the following disclaimer.
>
> 2. The origin of this software must not be misrepresented; you must not
>    claim that you wrote the original software. If you use this software in
>    a product, an acknowledgment in the product documentation would be
>    appreciated but is not required.
>
> 3. Altered source versions must be plainly marked as such, and must not be
>    misrepresented as being the original software.
>
> 4. The name of the author may not be used to endorse or promote products
>    derived from this software without specific prior written permission.
>
> THIS SOFTWARE IS PROVIDED BY THE AUTHOR ``AS IS'' AND ANY EXPRESS OR
> IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES
> OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN
> NO EVENT SHALL THE AUTHOR BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
> SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED
> TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
> PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
> LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
> NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
> SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
