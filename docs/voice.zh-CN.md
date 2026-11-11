# 语音

[English](voice.md) · **简体中文**

**说话。** 按 **⌘⇧S**（Linux 和 Windows 上是 Ctrl+Shift+S；可以改键，命令面板里叫 **说话**）或麦克风按钮，然后说出你想做的事。麦克风会一直开着，直到你再按一次其中任何一个：每次停顿就结束一句话，这句话在后台处理，同时继续听下一句。在 **设置 → 语音** 中下载 [Smart Turn](https://huggingface.co/pipecat-ai/smart-turn-v3)（9 MB）后，停顿会按你说最后几个词的方式来判断：说完的一句话在半秒内就会执行，句子说到一半停下来想一想，则最多会等三秒；它同样在这台电脑上、在 `clew-speak` 中运行。听到的内容要么是窗口的某个命令——按你平时的说法说即可，词序随意，带不带「open」「show」都行；有代价的命令（一次全部解释、启动或停止程序、打开另一个文件夹）会先问你，你可以直接说「是」或「否」来回答——要么是一个问题，会交给 Ask 并立即提问（这两项都可以在 **设置 → 语音** 中关闭）。语音在 Ask 所在的角落显示它正在做什么：小胶囊上显示它在听，执行一个命令后显示它做了什么，不会打开卡片；提出问题则会打开卡片并给出回答。

**窗口能做的任何事。** 命令面板里的每个命令，用窗口当前显示的语言说出它的名字就会执行；调试器的每一步（「step over」「continue」）也一样，而且不必每一步都确认。需要一个值的命令，把值一起说出来就行，用英语或窗口当前的语言都可以：

| 说 | 会发生什么 |
|---|---|
| 「go to line 30」「line thirty」 | 光标跳到当前文件的第 30 行 |
| 「scroll down」「scroll up 10 lines」「go to the end of the file」 | 文件滚动一屏、若干行，或滚到开头或结尾 |
| 「fold all」「unfold all」「fold here」 | 折叠或展开，和 **zM**、**zR**、**za** 一样 |
| 「breakpoint on line 12」 | 在第 12 行设置（或取消）断点 |
| 「watch total」「stop watching total」 | 把一个表达式加入调试器的监视，或移除它 |
| 「find quantity」「find next」「close find」 | 查找栏在当前文件里查找这些词 |
| 「search for quantity」 | 搜索视图在整个项目里查找 |
| 「open cart dot py」 | 打开项目里这个名字的文件 |
| 「go to apply discount」 | 打开这个名字的符号，不管它怎么拼写（`apply_discount`、`applyDiscount`） |
| 「voice settings」「full screen」 | 打开设置的那一页；或改变窗口 |
| 「change the app's language to French」「dark mode」「use one dark」「never read answers aloud」「use OpenAI」 | 修改设置，效果和在设置里操作它的控件一样——用窗口的任何一种语言说都可以（「把语言改成英文」「言語をフランス語に変更して」） |
| 「download」「not now」「trust and connect」「yes」「no」 | 回答最上层的对话框：按名字说它的按钮，用「是」或「否」回答它的问题 |
| 「open the third result」「the second one」「第三个」 | 按位置选中当前列表里的那一行 |
| 「close」「cancel」「never mind」 | 关掉最上层的东西，和 **Esc** 一样：对话框、菜单、时间旅行、查找栏、Ask |
| 「search by meaning for where the total is worked out」「filter files by cart」「walk me through checkout」 | 填好需要输入文字的视图并运行 |

**不用动手。** 窗口里显示的任何东西都可以说出来按下：对话框的按钮、设置里的开关或选项、Ask 里回答智能体问题的按钮、列表里的行——说它们上面的字；如果几行有同样的按钮，就说它所属的那一项的名字（「remove rust-analyzer」）。最上层的对话框会像接收鼠标一样接收你说的话；正在写的注释或条件会把你的话当作内容，直到你说出它的 **保存**。会删除或忘掉东西的操作会先等你说「是」，因为某个词可能听错了；需要你读一下的问题，在你能读到之前不会替你回答。这些只属于你：Ask 里的智能体能用命令面板的命令和设置，但永远按不了某个问题的 **允许**。

**全在本机。** 麦克风通过系统自己的音频接口读取（Core Audio、WASAPI、ALSA，PipeWire 和 PulseAudio 都支持 ALSA），音频重采样到 16 kHz、裁剪到有人声的部分，再由 whisper.cpp 在这台电脑上转成文字：Mac 上通过 Metal 用 GPU，其他平台用 CPU。不会发送任何内容；只有你选择提出的问题才会像打字提问一样发给你的语言模型。

**叫它的名字。** 在 **设置 → 语音** 中打开 **说出“Hey, Clew”时开始聆听**，并在那里下载它需要的 **关键词检测器**（33 MB：k2-fsa 的流式 Zipformer，支持英文和中文，Apache-2.0，按公布的大小和 SHA-256 核对）。之后，只要麦克风没有在聆听，它就会等待——按钮以强调色显示：说「Hey, Clew」，它就开始聆听，就像按了语音快捷键一样；一口气说「Hey, Clew, open the call graph」也可以。说「Bye, Clew」或「Stop listening」，它就回到等待；用名字叫起来的聆听，三分钟没有说话也会这样。语音快捷键和麦克风按钮照常可以开关聆听。同一时间只有一个窗口在等待：最前面的那个。等待期间，麦克风听到的声音只交给关键词检测器——它在这台电脑上的 `clew-speak` 中运行，只认得这一句：什么都不会保存，也不会转成文字。

**模型。** 使用 whisper.cpp 自己的模型，也就是 [Echo](https://github.com/damien-schneider/Echo) 使用的那些，只有在你点击某个模型的 **下载**（按钮上标有大小）时才会下载，并且只有与公布的文件逐字节一致才会被接受：**Small**（`small-q5_1`，190 MB，默认）、**Large Turbo**（`large-v3-turbo-q5_0`，574 MB）和 **Large**（`large-v3-q5_0`，1081 MB）按 Echo 的方式用 SHA-256 和精确大小核对，三者都支持多种语言；另有 Tiny 和 Base（英文），适合在较慢的机器上快速下命令，用 whisper.cpp 列出的 SHA-1 核对。模型保存在 Clew 的数据目录（`voice/`）中，也可以在那里删除。在 macOS 上，Clew 第一次聆听时，系统会询问是否允许它使用麦克风。

**朗读回答。** 你用语音提出的问题的回答——如果你愿意，也可以是每个问题的回答——会边写边读给你听，一次一句，每句一写完就开始读：代码块会跳过，句子里的代码会读成文字（`cart.py:12` 读作「cart dot py line 12」，`apply_discount` 读作「apply discount」）。它说话时，Ask 的角落会像它聆听时那样显示出来——扬声器、音量，以及亮起的按钮，按下它就停止；**Esc**、一个新问题和 Ask 的 **停止** 也会让它停下；语音快捷键会让它停下并开始聆听。麦克风开着时，你也可以像打断一个人那样直接打断 Clew：Clew 说话时麦克风保持打开，回声消除会把 Clew 自己的声音从它听到的内容里去掉（WebRTC 的 AEC3，Rust 实现，通过 [sonora](https://github.com/dignifiedquire/sonora)）。Clew 听到你的第一个词就会暂停，确认那是有人在说话——而不是它自己的声音被听回来——就停止，否则从暂停的地方继续。麦克风打开后 Clew 开始说话的头几秒，回声消除还在学习房间的声音，这时说的话不会让它暂停：**Esc** 和语音快捷键照样有效。在 **设置 → 语音** 中关闭 **Clew 说话时可以直接打断它**（比如扬声器声音很大时），麦克风就会在 Clew 朗读回答时等待，等 Clew 说完再打开。**设置 → 语音** 里的 **朗读回答** 决定朗读哪些回答：用语音提的问题的回答（默认）、所有回答（打字提的问题也算），或者都不读。声音同样在这台电脑上生成，由 `clew-speak`（和 Clew 放在一起的一个程序）中的 [ONNX Runtime](https://onnxruntime.ai) 完成，在系统默认的输出设备上播放——流式播放：一句话生成出前三分之一秒就开始播，其余部分边生成边播。如果电脑太慢，生成语音的速度赶不上播放，一句话只会等到生成的部分足够多、不会中途停下时再开始。

**声音。** 只有在你点击 **设置 → 语音** 中的 **下载** 时才会下载，并且只有每个文件都与 Clew 固定的那个版本公布的文件逐字节一致（按 SHA-256 和大小核对）才会被接受：**Qwen3-TTS**（[Qwen](https://huggingface.co/Qwen/Qwen3-TTS-12Hz-0.6B-CustomVoice) 的 0.6B 模型，自带多种声音，719 MB，Apache-2.0，由 [Quick Inference](https://huggingface.co/danyalmalik/Qwen3-TTS-12Hz-0.6B-CustomVoice-ONNX-WebGPU) 转换为 ONNX），能像真人一样说英语、中文、日语、韩语、德语、法语、西班牙语、葡萄牙语、意大利语和俄语——默认用每种语言自己的声音（英语是 Ryan，中文是 Vivian，日语是 Ono Anna，韩语是 Sohee），也可以在任何语言里用它七种声音中的任意一种；以及 **Supertonic 3**（[Supertone](https://huggingface.co/Supertone/supertonic-3)，145 MB，BigScience OpenRAIL-M），用于另外二十二种语言——其中包括荷兰语、波兰语、土耳其语、阿拉伯语和印地语——有十种声音，五女五男。每句话都用它自己的语言来说：中文、日语和韩语按文字判断，英语、法语、西班牙语、德语、意大利语、葡萄牙语和荷兰语按词语判断。**朗读回答**、声音和 **语速**（0.8× 到 1.5×）也在那里设置，**试听** 会播放一句话；它们保存在 Clew 的数据目录（`voice/speech/`）中。

语音在 macOS 和 Linux 上的表现一致，Clew 窗口里的其他功能也一样：整个应用在 CI 中于这两个平台上构建并测试，每个版本都为这两个平台发布。
