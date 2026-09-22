# AnySync 项目说明

本文档描述 AnySync 的基本背景。

## 背景

目前支持在配置中选择 `alist` 或标准 `webdav`，两者均通过 WebDAV 传输文件。AList 自动使用服务地址的域名和端口下的 `/dav` 作为 endpoint；标准 WebDAV 由用户指定 endpoint。`root` 是相对于 endpoint 的远程仓库路径。用户在本地初始化和配置后，通过 `pull`、`push` 完成多端同步。

## 代码结构

- `config.rs` 负责配置交互、持久化和仓库查找；地址解析交给后端模块。
- `sync.rs` 通过公共 `Backend` 接口执行同步，负责扫描、比较、进度展示及本地文件写入。
- `backend/mod.rs` 定义后端接口、公共文件信息和上传进度回调，并根据配置创建后端。
- `backend/alist.rs` 独立封装 AList 地址规则，目前委托 WebDAV 完成传输，后续 AList 特有行为在此实现。
- `backend/webdav.rs` 实现标准 WebDAV 请求、响应解析及重试。

公共接口覆盖递归列文件、下载、上传、确保仓库根目录存在和创建子目录。上传返回成功时的尝试次数；结果不确定时报错。上传进度表示请求体读取进度，文件暂未出现在远端列表不会触发自动重传。
