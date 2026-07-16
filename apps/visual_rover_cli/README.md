# @vrover/visual-rover-cli

The one-shot **CLI frontend** for the VRover GUI agent (the "brain"). Wires config → LLM
provider → Platform → (optional) native OmniParser → the `runAgent` observe→think→act loop,
then prints the result and exits.

## Usage

```bash
pnpm --filter @vrover/visual-rover-cli start -- --task "click the login button"   # mock platform (default)
pnpm --filter @vrover/visual-rover-cli start -- --platform mock --provider glm --task "log in"
pnpm --filter @vrover/visual-rover-cli start -- --platform remote --scout-port 9000   # drive a Visual Scout server
pnpm --filter @vrover/visual-rover-cli start:desktop                              # = start --platform desktop
pnpm --filter @vrover/visual-rover-cli start -- --platform desktop --yolo-path weights/icon_detect.onnx
```

Without `--task` it prompts on a TTY, or reads the task from stdin. See `--help` for all flags.

The web frontend lives in `@vrover/visual-rover-web`.
