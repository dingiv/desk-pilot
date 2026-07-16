---
title: 链接库
order: 60
---

# 链接库
动态链接的麻烦，几乎从来不在"链接"本身，而在于它被拆成了两个阶段：链接期让编译通过，运行期让操作系统能把库加载进来。这两个阶段各自独立，解决了前一个并不自动解决后一个——而"在我机器上能跑、一交付就闪退 shared library not found"的事故，几乎都出在运行期这一段。所以管理动态库依赖的第一性原则只有两条：能静态链接就别动态，必须动态时就让二进制自己能找到自己的库。

## 两阶段：链接期与运行期
链接期的工作在 build.rs 里完成：用 `cargo:rustc-link-search` 告诉 rustc 去哪里找库，用 `cargo:rustc-link-lib=dylib=foo` 指定要链接的库名（`libfoo.so` 对应链接名 `foo`）。这一步只保证编译和链接成功，产物里记下的只是"我需要 libfoo.so"，至于运行时去哪儿找它，rustc 一概不管。

运行期才是事故高发区，这里有一个流传很广的误区：在 build.rs 里写 `cargo:rustc-env=LD_LIBRARY_PATH=...` 并不会让 `cargo run` 找到库。`rustc-env` 设置的环境变量只在编译期生效，供 `env!` 宏读取，它不会进入运行期进程的环境，更不会被动态加载器看到。Cargo 至今也不会为 `cargo run` 自动拼装 `LD_LIBRARY_PATH`（这是 [cargo#4895](https://github.com/rust-lang/cargo/issues/4895) 长期悬而未决的需求）。指望 build.rs 一条指令同时搞定链接和运行，是这个领域最常见的踩坑点。

## 运行期：让二进制自定位
优雅的做法是不依赖任何环境变量，把搜索路径直接烧进二进制，让它"相对自己"去找库。这样无论被拷到哪台机器、哪个目录，行为都一致。

Linux 和 macOS 上靠 rpath（Runpath）实现。在 `.cargo/config.toml` 里按目标平台传链接参数，用 `$ORIGIN`（Linux）或 `@executable_path`/`@loader_path`（macOS）表示可执行文件自身所在目录：

```toml
# .cargo/config.toml
[target.x86_64-unknown-linux-gnu]
rustflags = ["-C", "link-arg=-Wl,-rpath,$ORIGIN/lib"]
```

`$ORIGIN/lib` 的含义是"去我旁边的 lib 子目录里找"。只要分发时保持 `myapp` 与 `lib/` 同级，程序在任何地方都能直接启动。

这里有个容易翻车的细节：现代链接器默认生成 DT_RUNPATH（`--enable-new-dtags`），它只对二进制直接依赖的库生效，不会传递给间接依赖。如果 `libfoo.so` 自己又依赖 `libbar.so`，单靠顶层二进制的 rpath 是找不到 `libbar.so` 的——要么让链上每个 `.so` 都带上自己的 RUNPATH，要么退回 DT_RPATH（`-Wl,--disable-new-dtags`）。这种传递性缺口，是 rpath 方案最常见的"明明配了却还是找不到"。

Windows 没有 rpath 机制，加载器按固定顺序找 DLL：可执行文件同级目录 → 系统目录 → PATH。所以 Windows 上的优雅做法最简单——把 `.dll` 和 `.exe` 放在同一个目录里，加载器天然就能找到，不需要任何配置。

## 统一开发与部署：同一个机制
很多人把开发期和部署期当成两套独立配置去维护（开发设环境变量、部署再改 rpath），这其实把问题复杂化了。更优雅的思路是让两个阶段共用同一个机制——rpath——只在取值上按 profile 分流。

在 build.rs 里根据 `PROFILE`（`debug` 或 `release`）给出不同的 rpath：开发期用指向工作区 `lib/` 的绝对路径，这样每个开发者的 `cargo run`、`cargo test` 开箱即用、零配置、且不污染宿主环境；发布期用 `$ORIGIN/lib` 这样的相对路径，让产物可移植。绝对路径只烧进从不交付的 debug 产物，因此不会带来可移植性问题。

```rust
// build.rs（节选）
use std::path::PathBuf;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let lib_dir = PathBuf::from(&manifest_dir).join("lib");

    // 链接期：告知 rustc 库的搜索路径与名称
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=dylib=foo");
    println!("cargo:rerun-if-changed=lib/");

    // 运行期：按 profile 分流 rpath，开发期绝对路径、发布期相对路径
    let profile = std::env::var("PROFILE").unwrap();
    let rpath = if profile == "release" {
        "$ORIGIN/lib".to_string()
    } else {
        lib_dir.display().to_string()
    };
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", rpath);
}
```

无论开发还是部署，核心约束只有一条：库要待在二进制找得到它的相对位置上。部署时要么把 `lib/` 和可执行文件打包在一起（配合 `$ORIGIN/lib`），要么在 Windows 上扁平地堆在同级目录。坚决避免在生产环境改宿主机的 `LD_LIBRARY_PATH` 或 `/etc/ld.so.conf`——那会把程序和宿主机上其他进程耦合在一起，一旦库版本冲突就是全机事故。

## 嵌入单文件：最后的手段
有时确实需要把 `.so` 直接打进二进制、对外只交付一个文件。`include_bytes!` 能在编译期把库的字节数组嵌进产物，但加载方式决定了它可不可靠。

直接内存加载（`dlopen-rs`、Windows 的 MemoryModule）听上去最干净，实则很脆：Linux 的 `dlopen` 原生不支持内存加载，这类库要在用户态手写一个 ELF 链接器，遇到依赖复杂 C++ 库的 `.so` 极易崩；Windows 下内存加载 DLL 又是典型的免杀手法，容易被杀软拦截。

工业上更稳的是"编译期嵌入 + 运行期释放 + 原生加载"：把字节写到临时目录，再用 `libloading` 走操作系统原生的 `dlopen`/`LoadLibrary` 加载，借助 RAII 在程序退出时自动清理临时文件。它兼容性最好，不论库多复杂都不会链接失败。

但说到底，如果手里有库的源码，最该做的不是嵌入而是静态链接：在 build.rs 里把源码编成 `.a`/`.lib`，用 `cargo:rustc-link-lib=static=foo` 揉进二进制。既不需要运行期解析，也不需要嵌入手段，直接得到一个纯粹的单文件产物。嵌入只留给"只有二进制 blob、又必须单文件交付"这一种情况。

## 工程防线
动态库管理的最后一道关，是用流程把"漏包"挡在交付之前。

写 build.rs 时，链接指令务必显式带上类型：`cargo:rustc-link-lib=dylib=foo`，而不是省略成 `=foo`。后者在某些环境下会被编译器自作主张地静态链接，从而在测试阶段掩盖掉"运行期根本找不到库"的 bug——这种假绿灯是最危险的。

CI 流水线的收尾加一道依赖校验：对 release 产物跑 `ldd ./target/release/myapp | grep "not found"`，一旦命中就让流水线直接挂掉，绝不把缺库的制品放行到测试或生产。

更彻底的做法是干净沙箱集成测试：在一个不含任何开发库、不设任何开发环境变量的空白容器里运行 release 产物。它能跑通，就证明 rpath 或打包策略确实自洽，可以放心交给部署。
