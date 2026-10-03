# weflow-server TypeScript 示例客户端

**仅示例，不发布 npm**：TypeScript 当前没有真实消费者，本目录的定位是
「可运行的最小参考」，不承诺兼容性、不进任何发布渠道。

## 内容

- `client.ts` —— 类型化示例客户端：`health` / `accounts` / `wait_ready`
  （wait-only 就绪轮询，不做任何注册动作）/ `ensure_ready`（注册 + 注册
  应答里 200 拒绝态的快速失败，拒绝词表与 Rust/Python 行为层一致）/
  `watch`（SSE 单连接连续多帧：字节级 LF 分帧、`id:`/`event:`/`data:`
  组装、空行成帧、`Last-Event-ID` 续读、指数退避、1 MiB 缓冲上限——超限帧
  不交付并退避重连）。演示级子集：分帧与退避的**形状**与 Rust/Python 行为层
  对齐，不承诺逐语义一致、不承诺兼容性。
- `smoke.ts` —— 可执行冒烟：对真实服务发请求并**真断言** HTTP 结果
  （「import 成功」不算冒烟）。第 3 步只接受两种合法结局——业务拒绝
  （`StatusError`，消息带 state）或受理后就绪等待超时（`NotReadyError`）；
  其它结局（415 / 500 / 网络错误 / 未知异常）一律非零退出。凭据从环境
  变量注入，不使用真实数据。
- `stub-server.mjs` —— 入库的最小假服务：假的是**形状**，不是断言。没有
  真实服务时用它跑 smoke；把注册应答改成 500（`STUB_STATUS=500`）时同一条
  smoke **必须**非零退出——这条自证让「PASS」成为可失败的断言。

## 运行

要求 Node **18+**（fetch 内置；更早版本需要额外 flag，不再支持）。

```bash
npm install --include=dev    # 仅 devDependencies：typescript 与 @types/node
                             # （NODE_ENV=production 的环境必须带 --include=dev，
                             #  否则 npm 会静默跳过 devDependencies、tsc 不存在）
npm run typecheck            # tsc --noEmit
npm run build                # tsc 编译到 dist/

# 对真实服务：
WEFLOW_BASE_URL=http://127.0.0.1:5033 WEFLOW_API_TOKEN=<token> node dist/smoke.js

# 对入库 stub（无需真实服务）：
node stub-server.mjs &                              # 端口 5099，默认注册=业务拒绝
WEFLOW_API_TOKEN=fictional node dist/smoke.js       # 必须 PASS
# 自证（改 stub 应答后必须变红）：
STUB_STATUS=500 node stub-server.mjs &
WEFLOW_API_TOKEN=fictional node dist/smoke.js       # 必须非零退出
```

两仓的 TS 示例等价：同一组端点与结局语义；`watch` 本仓是异步生成器、
qqflow 版是回调（形状差异，非语义差异）。

## 边界

不修改 `release.yml` 的产物矩阵：TypeScript 侧保持「本机可用 + 示例」，
不新增任何发布产物。
