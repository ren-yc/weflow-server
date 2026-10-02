# weflow-server TypeScript 示例客户端

**仅示例，不发布 npm**：TypeScript 当前没有真实消费者，本目录的定位是
「可运行的最小参考」，不承诺兼容性、不进任何发布渠道。

## 内容

- `client.ts` —— 类型化示例客户端：`health` / `accounts` / `wait_ready`
  （wait-only 就绪轮询，不做任何注册动作）/ `ensure_ready`（注册 + 注册
  应答里 200 拒绝态的快速失败，拒绝词表与 Rust/Python 行为层一致）/
  `watch`（SSE，字节级 LF 分帧骨架）。与 Rust/Python 行为层同构的最小子集。
- `smoke.ts` —— 可执行冒烟：对真实运行的服务发请求并断言 HTTP 结果
  （「import 成功」不算冒烟）。凭据从环境变量注入，不使用真实数据。

## 运行

```bash
npm install --include=dev    # 仅 devDependencies：typescript 与 @types/node
                             # （NODE_ENV=production 的环境必须带 --include=dev，
                             #  否则 npm 会静默跳过 devDependencies、tsc 不存在）
npm run typecheck            # tsc --noEmit
npm run build                # tsc 编译到 dist/
WEFLOW_BASE_URL=http://127.0.0.1:5033 WEFLOW_API_TOKEN=<token> node dist/smoke.js
```

## 边界

不修改 `release.yml` 的产物矩阵：TypeScript 侧保持「本机可用 + 示例」，
不新增任何发布产物。
