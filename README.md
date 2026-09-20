# AnySync

基于 WebDAV 的极简多端文件同步工具。

## 用法

```bash
anysync init              # 在当前目录初始化仓库（创建 .anysync/）
anysync config              # 交互式配置 WebDAV 地址、根目录、用户名、密码

anysync config --url <webdav-url>
anysync config --root <root-path>
anysync config --username <username>
anysync config --password   # 交互式输入密码，不回显

anysync pull              # 拉取远端较新的文件
anysync push              # 推送本地较新的文件
```

## 行为说明

- 以文件修改时间（mtime）判断新旧，容差 2 秒；较新的一方不会被较旧的一方覆盖。
- 修改时间相同但大小不同时视为冲突，跳过并提示。
- 删除不会被传播：本地删除的文件会在 pull 时被拉回，远端删除的文件会在 push 时重新上传。
- `.anysync/` 与 `.git/` 不参与同步；空目录不同步；符号链接默认跳过。
- `--root` 会追加到 `--url` 的路径后面；例如 URL 为 `https://host/dav`、root 填 `/dav/test` 时，实际仓库根目录是 `/dav/dav/test`，push 会自动递归创建缺失的远端目录。
- 修改时间相同、大小相同的文件视为一致，重复执行 pull/push 不会重复传输。
- push 会在终端显示当前文件的上传进度；网络错误、409、429 和 5xx 等瞬时错误会自动重试，上传后未及时出现在远端列表中的文件也会有限次数重新上传。

## 安全说明

- 密码不会作为命令行参数传入（避免被 shell 历史或进程列表记录）。
- 密码明文保存在本地 `.anysync/config.toml` 中，请勿提交该目录；未保存密码时，pull/push 会提示输入。
