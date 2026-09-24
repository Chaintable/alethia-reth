# Taiko Alethia-Reth v1.4.1 upstream merge 验证

## 1. 发布内容与升级判断

[官方 v1.4.1 release](https://github.com/taikoxyz/alethia-reth/releases/tag/v1.4.1) 标记为 prerelease，没有主网分叉时间或区块执行规则变更，也没有要求立即升级；本次不属于主网规则强制升级。合入价值是继承 Reth v2.4.1 相关依赖更新、`debug_trace*` / `trace_*` 的 anchor 交易回放修复、`eth_estimateGas` 的逐调用 zk gas 计量及 tracer 的 log/selfdestruct 转发。当前生产 fork tag 已包含 tracer 转发的部分改动，因此不能把 release 全部修复都算作相对生产的新收益。建议完成本 PR 审阅后按正常窗口升级，生产切换仍由 SRE 单独执行。

v1.4.1 覆盖 v1.3.1 以来的变更；v1.4.0 因旧 `Cargo.lock` 导致镜像构建失败，没有发布可用镜像。主网 preset 保留，Masaya preset 移除；当前主网部署未使用 Masaya。官方说明现有 Reth 数据库无需迁移或重新同步。discv5 默认开启、proof-history 指标改名等运维变化需按实际配置检查；当前生产 node 未启用可选 proof-history。

## 2. 合并冲突与影响面

从 fork `main@7fdcf15c` 合并上游 `v1.4.1@0fb47d96`，merge commit 为 `6c997ffe41b26b3bb16cfdcf706fcad7a2056668`。冲突涉及 `.github/workflows/ci.yml`、`Cargo.toml`、`Cargo.lock` 和 `crates/evm/src/zk_gas/adapter.rs`。合并保留 Chaintable 的多架构构建及发布流程、DeBank `trace_debankBlock` RPC 与其 fork 依赖；为上游的新 Reth/REVM API 调整 fork RPC 实现及测试夹具。`Cargo.lock` 以上游 v1.4.1 为基底，仅补 fork 所需依赖，未全量更新传递依赖。

影响面主要是节点执行依赖、trace/debug/estimateGas 及 fork 的 DeBank trace RPC。生产通过独立 ETL sidecar 调用 `trace_debankBlock` 并投递；node 自身没有 S3/Kafka 投递客户端。本次保留原有 node+driver 接口，ETL 与生产投递配置不在 PR 内。

## 3. 部署情况

- 测试机：`lihe-dev`，`ap-northeast-1a`；生产参考为 `chaintables/blockchain-taiko` 的 seed node。
- 数据：从生产 seed 快照 `snap-02e876bcdbe04ebb6` 创建隔离测试卷 `vol-0c9e3802b15401940`，131 GiB gp3，初始化速率 300 MiB/s；挂载到 `/opt/app/taiko/writer_merge_reth-v1.4.1/data`。挂载后 ext4 文件系统 128 GiB，初始已用 105 GiB、可用 24 GiB。
- 测试 node 镜像：`public.ecr.aws/b2h7a5c4/chaintable/taiko-writer:2df25dc2`，由 PR #3 的合成 merge commit 构建；其 tree 与 PR head 相同。driver 沿用生产 `taiko-alethia-client-v2.6.0`。
- 测试仅运行 node + driver，独立 bridge 网络 `10.99.74.0/24`，RPC 仅绑定测试机回环地址；不运行 ETL、jrpcx 或投递服务，不连接生产 S3/Kafka。清除快照中的 node/driver P2P 身份，生成本次独立 JWT。driver 使用可读完整 L1 历史的 `--l1.http=http://archive.eth.blockchain` polling；生产目前使用的 `archive.eth-ws.blockchain` 在旧快照启动时返回 `pruned history unavailable`，生产配置未改。

测试 Compose 全文：

```yaml
name: taiko-reth-v141
services:
  node:
    image: public.ecr.aws/b2h7a5c4/chaintable/taiko-writer:2df25dc2
    container_name: taiko-reth-v141-node
    entrypoint:
    - /app/alethia-reth
    command:
    - node
    - --chain=mainnet
    - --datadir=/var/data
    - --config=/var/data/reth.toml
    - --p2p-secret-key=/var/data/alethia-p2p-secret
    - --bootnodes=enode://266a8e3b5e44201eca9c368d58aa59a7750295397e77d5b32aea2644f9962cbc4e1cb0543aab0480995a209408174413f65e5ce253d60bb83d22d3b8ab12eb89@34.142.239.251:30303,enode://264a7fc4bd1ee16cfc6eb420c643407bfc61b9c9534c5a39ba6e68c8759beda2fbeccefee8677385e3d99691eeb218da4bce7f5207cf38594ac0f6a53c128b9b@35.247.159.156:30303,enode://2d4e5b7ec0c57f9def6ebe72f9bd1f65c33c87b7dc38875bbb147c10e8ec9a8cd157558b695f9a02ac6ad789f300fab4f1f19d41273956491372e96880a3459f@34.126.90.255:30303,enode://57f4b29cd8b59dc8db74be51eedc6425df2a6265fad680c843be113232bbe632933541678783c2a5759d65eac2e2241c45a34e1c36254bccfe7f72e52707e561@104.197.107.1:30303,enode://87a68eef46cc1fe862becef1185ac969dfbcc050d9304f6be21599bfdcb45a0eb9235d3742776bc4528ac3ab631eba6816e9b47f6ee7a78cc5fcaeb10cd32574@35.232.246.122:30303,enode://27384375dbbfd8af5df71392fceb01b93cef1f674e4884fb481be145426f9ce5706941fd93928101a3079a52e621d8a42d266ab9165f4f960233adce22e905f8@34.21.195.34:30303
    - --port=30514
    - --discovery.port=30514
    - --enable-discv5-discovery
    - --discovery.v5.port=19211
    - --http
    - --http.addr=0.0.0.0
    - --http.port=8545
    - --http.api=eth,net,debug,trace,rpc,web3
    - --ws
    - --ws.addr=0.0.0.0
    - --ws.port=8546
    - --ws.api=eth,net,debug,rpc,web3
    - --rpc.max-response-size=512
    - --rpc.max-tracing-requests=4
    - --rpc.max-blocking-io-requests=16
    - --authrpc.addr=0.0.0.0
    - --authrpc.port=8551
    - --authrpc.jwtsecret=/var/data/jwt.hex
    - --metrics=0.0.0.0:6060
    - --ipcdisable
    - --engine.persistence-threshold=128
    - --engine.memory-block-buffer-target=128
    - --engine.cross-block-cache-size=512
    - --log.file.directory=/var/data/logs
    - --log.file.max-size=100
    - --log.file.max-files=5
    - --color=never
    restart: unless-stopped
    stop_signal: SIGTERM
    stop_grace_period: 3m
    mem_limit: 12g
    cpus: 4
    volumes:
    - /opt/app/taiko/writer_merge_reth-v1.4.1/data:/var/data
    ports:
    - 127.0.0.1:28545:8545
    - 127.0.0.1:28660:6060
    - 30514:30514/tcp
    - 30514:30514/udp
    - 19211:19211/udp
    logging: &id001
      driver: json-file
      options:
        max-size: 100m
        max-file: '5'
    networks:
    - taiko-reth-test
  client:
    image: us-docker.pkg.dev/evmchain/images/taiko-client:taiko-alethia-client-v2.6.0
    container_name: taiko-reth-v141-client
    entrypoint:
    - taiko-client
    command:
    - driver
    - --l1.http=http://archive.eth.blockchain
    - --l1.beacon=http://archive-beacon.eth.blockchain
    - --l2.ws=ws://node:8546
    - --l2.auth=http://node:8551
    - --inbox=0x6f21C543a4aF5189eBdb0723827577e1EF57ef1f
    - --taikoAnchor=0x1670000000000000000000000000000000010001
    - --verbosity=3
    - --jwtSecret=/var/data/jwt.hex
    - --p2p.sync
    - --p2p.checkPointSyncUrl=https://rpc.mainnet.taiko.xyz
    - --p2p.peerstore.path=/var/data/client/peerstore
    - --p2p.discovery.path=/var/data/client/discv5
    - --preconfirmation.serverPort=9871
    - --preconfirmation.jwtSecret=/var/data/jwt.hex
    - --p2p.listen.ip=0.0.0.0
    - --p2p.listen.tcp=19333
    - --p2p.listen.udp=19334
    - --p2p.useragent=taiko-merge-reth-v1.4.1
    - --p2p.bootnodes=enode://c263741b17759f3850d24d67d6c3cbc307c73e17d80c6b12a63a4792a10529d1125d00ecf7ef4c9b0dc51d28b94dfc1b8798fb524f61a1f93946748649f73b23@34.142.239.251:4001?discport=30304,enode://2f37c3affd83274b262fa2a259d32d41510dd5a48d6e916696efe7f1598cb3f905305f5989e7b6607aab50697fb2e52cb4b6904116ed67cc5fcea1e6d66ccaba@35.247.159.156:4001?discport=30304,enode://dd83dedeff622ecfca0c5edf320266506c811539a553ddd91589cdfcc9bbd74d0d620f251d8d5e1180f19a446abbdd8b6b5301e9aa6cbad35cfd9716f80f2416@34.126.90.255:4001?discport=30304,enode://0e917d0fd54e25cd9815aa552550c938dfccafb1b09e613d1b5f344236dc2a93cdac9508a9e570a4e1a315ca7762de51f41c4a90361519d972fdd6715bb3f4f3@51.210.195.167:4001?discport=30303,enode://0e917d0fd54e25cd9815aa552550c938dfccafb1b09e613d1b5f344236dc2a93cdac9508a9e570a4e1a315ca7762de51f41c4a90361519d972fdd6715bb3f4f3@34.248.16.142:4001?discport=30303,enode://67c739e00a9b6476fce8f9d3cf0ae12589c325c51520da75c2dbfe5094d5c9ef72abf0e368316b10e8332b96e1886869164113a86b66f7f8b989ed0d8dee8895@34.126.181.0:4001?discport=30304,enode://a31855cd5c4138327617bde2e87418774d03c40cd7217344e24f6d906a8ca72c777d65ed9bd38b20e03fa5f91302ae690b00963cd490011c549189ac24e0d1b9@34.21.195.34:4001?discport=30304
    - --p2p.priv.path=/var/data/client/p2p.key
    - --p2p.nat
    restart: unless-stopped
    stop_signal: SIGTERM
    stop_grace_period: 1m
    mem_limit: 2g
    cpus: 2
    depends_on:
    - node
    volumes:
    - /opt/app/taiko/writer_merge_reth-v1.4.1/data:/var/data
    ports:
    - 19333:19333/tcp
    - 19334:19334/udp
    - 127.0.0.1:29873:9871
    logging: *id001
    networks:
    - taiko-reth-test
networks:
  taiko-reth-test:
    driver: bridge
    ipam:
      config:
      - subnet: 10.99.74.0/24
```

## 4. 部署后测试情况

- 本地：固定 Rust 1.95.0 的全 workspace、all-features `cargo nextest` 353/353 passed，严格 Clippy、nightly rustfmt 检查通过；Rust 1.96.1 的全 workspace check/测试/Clippy 同样通过。PR #3 amd64、arm64、manifest CI 均通过。
- 启动与同步：测试节点初始高度 `11,537,605`，该块 hash `0xf9226884a55f6478752f8f781c2773123d6fdff3215452d266670c3be178ff00`、state root `0x8fa46bc5f472c5327cbc946996fda31de644cd3e30e722eba8bb4521575903ab` 与生产 seed 一致。driver 于 2026-09-24 08:00:37 UTC 触发追块；第一轮 14 个同步阶段于 08:32:59 UTC 完成到 `11,802,300`。之后连续追块，08:37–08:38 UTC 两次采样中测试与生产 seed 高度相同，第二次均为 `11,802,819`，测试 `eth_syncing=false`，node/driver 均无重启或 OOM。
- 块与状态：快照末端 `11,537,585..11,537,604`、新追入边界 `11,537,606..11,537,625`、近 head `11,802,650..11,802,669`，三段各 20 块的 hash 和 state root 均与生产 20/20 一致；另核对持续追块后的 `11,802,764`，hash/state root 一致。
- DeBank trace：快照段 `11,537,520`（5 笔交易、10 条 trace）、`11,537,590`、`11,537,600`、`11,537,604`，追块段 `11,537,606` 和 `11,802,665`（6 笔交易、20 条 trace），共 6 个 `trace_debankBlock` 响应。仅排除每次请求生成的 `block_file.block.process_start_timestamp`，其余完整 JSON 与生产逐字段一致。快照段 `11,537,600` 和追块段 `11,802,665` 各取一笔交易，`eth_getTransactionReceipt` 完整 JSON 一致。

结论：本次覆盖的主网 node+driver 同步、块 hash/state root、receipt 与 `trace_debankBlock` 均通过。可选 proof-history 未启用，ETL/S3/Kafka 投递由独立 sidecar 承担且本次未运行；上述结论不包含这两部分的动态验证，也不构成生产 rollout 授权。

## 5. 过程中暴露的其他问题

- 首轮追平后，新一轮约 363 块处理期间，Reth 输出 363 条 `Changeset cache MISS in range`。固定依赖 `reth@f2eecc6` 的 `crates/trie/db/src/changesets.rs` 在缓存未命中时转为基于数据库的聚合计算；该轮最终完成，之后日志未再出现同类告警，已抽样的状态根与 trace 一致。首次追平后的这类告警仍值得在生产升级观察。
- 测试 driver 在 08:33:19 UTC 一次报告 `Failed to fetch core state for finalized checkpoint ... flatdb reader not initialized`，日志同时说明继续插入区块。之后继续追平且近 head 对照通过，近几分钟未复现；触发该请求的精确条件尚未查明，不能把它记为已修复的代码缺陷。
