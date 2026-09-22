# AnySync

基于 WebDAV 的极简多端文件同步工具。

## 用法

```bash
anysync init              # 在当前目录初始化仓库（创建 .anysync/）
anysync config           # 交互式选择后端、配置地址、根目录和认证

anysync config --backend alist --url https://host --root /test
anysync config --backend webdav --url <webdav-endpoint>
anysync config --root <root-path>
anysync config --username <username>
anysync config --password   # 交互式输入密码，不回显

anysync status              # 只读查看同步状态

anysync pull              # 拉取远端较新的文件
anysync push              # 推送本地较新的文件
```

## 行为说明

- 在需要同步的本地目录运行 `init`、`config`，然后自行执行 `pull` 或 `push`。
- 配置时选择 `alist` 或 `webdav`。AList 根据服务地址自动使用 `协议://域名[:端口]/dav` 作为 endpoint；标准 WebDAV 需要输入完整 endpoint。AList 部署在自定义路径时可选择 `webdav` 并手动指定 endpoint。
- 配置保存在 `.anysync/config.toml` 的 `[remote]` 下，`backend` 为 `alist` 或 `webdav`，`url` 保存 endpoint。旧配置没有 `backend` 时按 `webdav` 处理。
- 以文件修改时间（mtime）判断新旧，容差 2 秒；较新的一方不会被较旧的一方覆盖。
- 修改时间相同但大小不同时视为冲突，跳过并提示。
- 删除不会被传播：本地删除的文件会在 pull 时被拉回，远端删除的文件会在 push 时重新上传。
- `.anysync/` 与 `.git/` 不参与同步；空目录不同步；符号链接默认跳过。
- `--root` 是相对于 endpoint 的远端仓库路径；例如 AList 服务地址为 `https://host`、root 为 `/test` 时，实际仓库地址为 `https://host/dav/test`。root 为 `/` 时使用 endpoint 本身；push 会递归创建缺失的远端目录。
- 修改时间相同、大小相同的文件视为一致，重复执行 pull/push 不会重复传输。push 成功后会把本地文件的修改时间对齐为远端时间，刚推送的文件因此不会在随后的 status 里显示为待拉取。
- push 会在终端显示当前文件的上传进度；上传完成后只执行一次全量 `PROPFIND`，用于回读可见文件的远端 mtime。服务端尚未让文件出现在列表时，不重传、不将 push 判为失败，后续 push 或 pull 可继续处理。
- 传输过程中，仅在服务器明确返回 408、409、425、429 或 5xx 时，以及连接尚未建立时，才会最多自动重试 3 次；请求结果不确定时会停止并报告，避免服务端生成同名副本。

## 安全说明

- 密码不会作为命令行参数传入（避免被 shell 历史或进程列表记录）。
- 密码明文保存在本地 `.anysync/config.toml` 中，请勿提交该目录；未保存密码时，pull、push 和 status 会提示输入。
