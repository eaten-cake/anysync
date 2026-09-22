# AnySync 使用说明

本文档描述 AnySync 的基本用法。

## 仓库初始化

```bash
anysync init
```

该命令会在当前目录下创建`.anysync`目录，并完成仓库初始化。

## 远端仓库设置

一次性配置远程仓库时，直接运行：

```bash
anysync config
```

该命令依次询问存储后端、地址、远端根目录、用户名和密码：

- `alist`：输入 AList 服务地址，例如 `https://host:5244`。自动使用同一协议、域名和端口下的 `/dav` 作为 endpoint，输入地址中的原有路径会被替换。
- `webdav`：手动输入完整 WebDAV endpoint，例如 `https://host/remote.php/dav/files/user`。路径保持不变。AList 使用自定义部署路径时，也可选择此项。

远端根目录相对于 endpoint，例如 `/test`；填写 `/` 表示 endpoint 本身。AList 地址 `https://host` 加 root `/test`，最终访问 `https://host/dav/test`。root 不需要重复填写 WebDAV 入口的 `/dav`，除非存储内确实存在名为 `dav` 的目录。

用户名和密码是否需要填写，取决于远程仓库是否启用认证。配置保存到 `.anysync/config.toml` 的 `[remote]`：`backend` 指定后端，`url` 保存解析后的 endpoint，`root` 保存远端仓库路径。没有 `backend` 的旧配置按 `webdav` 处理，保持原有路径含义。

也可以只更新某一项配置：

```bash
anysync config --backend alist --url https://host --root /test
anysync config --backend webdav --url <webdav-endpoint>
anysync config --url <server-url-or-endpoint>
anysync config --root <root-path>
anysync config --username <username>
anysync config --password
```

指定参数时更新对应配置；地址会根据所选后端规范化。切换后端时建议同时指定 `--url`，根目录和认证信息不会自动改变。

执行 `--password` 后，程序应通过交互式提示读取密码，输入时不回显密码内容。密码不应作为命令行参数传入，以免被 shell 历史记录或进程列表记录。

## 在另一台设备开始同步

先创建或进入需要同步的本地目录，运行：

```bash
anysync init
anysync config
anysync pull
```

配置相同的远端后，即可拉取已有文件；本地修改后运行 `anysync push`。

## 查看同步状态

```bash
anysync status
```

该命令为只读操作：它不会创建远端目录，不会上传、下载或删除文件，也不会写入本地状态。输出按路径列出待推送、待拉取和冲突，并给出汇总。

## 拉取远端更改

```bash
anysync pull
```

该命令会将远程仓库中的更改拉取到本地仓库。

按照文件修改日期判断是否需要拉取：
- 本地仓库不存在、但远程仓库存在的文件，拉取
- 本地文件早于远程文件时，拉取远程文件
- 本地文件较新时，不覆盖本地文件

## 提交本地更改

```bash
anysync push
```

该命令会将本地仓库中的更改提交到远程仓库。

按照文件修改日期判断是否需要提交：
- 远程仓库不存在、但本地仓库存在的文件，提交
- 远程文件早于本地文件时，提交本地文件
- 远程文件较新时，不覆盖远程文件

上传结束后，push 会把本地文件的修改时间对齐为远端返回的修改时间。WebDAV 的 PUT 无法指定修改时间，服务端会把它置为接收时刻；不对齐的话，刚推送的文件会在随后的 status 里被判为「待拉取」。远端修改时间不可用、远端大小与本地不一致、或该文件在 push 期间被改动时，跳过对齐。对齐失败只提示，不影响 push 的成败。

上传完成后只执行一次全量 PROPFIND，用于回读当前可见文件的远端 mtime。服务端尚未让文件出现在列表时，push 不重传该文件，也不因此报错；后续 push 或 pull 会继续处理。

服务器明确返回 408、409、425、429 或 5xx 时，以及连接尚未建立时，最多自动重试 3 次；请求结果不确定时停止并报告，避免生成同名副本。
