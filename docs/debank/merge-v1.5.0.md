# Taiko Alethia-Reth v1.5.0 upstream merge 验证

## 1. 发布内容与升级判断

[官方 v1.5.0 release](https://github.com/taikoxyz/alethia-reth/releases/tag/v1.5.0) 建议所有运营者升级；这是常规维护版，没有分叉排期或 Taiko 区块执行规则变化。建议完成隔离验证与 PR 审阅后按常规窗口升级，不把它当作带 deadline 的紧急分叉升级。

主要变更包括 Reth 从 v2.4.0 加补丁升级至 OP Reth `op-v2.6.0-2`、REVM 41→43、alloy-evm 0.37→0.39、alloy 2.2→2.4；CALL/CREATE 与预编译的 traced zk gas 计量对齐正常执行；pre-Unzen 历史执行通过 provider 串行计算 state root；txpool 选取绕过 zk gas 超限交易继续打包；proof-history v2 重建及可恢复回填。构建要求 Rust 1.99。

当前生产为普通派生 node + 独立 ETL：更相关的是 traced execution 一致性、历史重放修复及底座维护；txpool 打包和 proof-history 并非当前 writer 的主要收益。生产未启用 `--proofs-history`，此次不会因该功能升级而触发 v2 store 重建。现有 engine persistence/buffer 均为128，满足新版参数校验；未使用被删除的 sparse-trie 参数。默认 state masking 为0，非0配置会拒绝启动。主 Reth 数据库不要求迁移或 resync。

最新 metadata 的其他三项 geth v2.8.0、client v2.8.0、simple-taiko-node v2.6.5 均无新稳定版。官方 bundle v2.6.5/main 的 Reth pin 仍是 v1.4.1，client pin 是 v2.8.0；本次独立采用已正式发布的 Reth v1.5.0，不把 bundle 旧 pin 写成已更新。

## 2. 合并冲突与影响面

生产 `v1.4.1-ct.1` 与合并基线 `origin/main@0a3e6f04` 完全相同；上游目标 `v1.5.0@6e86a39b`。增量18 commits、48文件，合并有11个冲突文件：8个CI/脚本，3个Cargo文件。

保留 fork 的公共 ECR 双架构 build/release workflow，供 Chaintable 镜像交付；保留 DeBank RPC、Geth 错误格式、transport timeout/取消、EOA/storage/EIP-7702行为及 fixture。Cargo.lock 以上游 v1.5.0 为基底，只补 fork 依赖：新增md5 0.7.0和两个本地crate依赖列表，未移除或漂移任何上游传递依赖；fork额外Reth crate全部指向同一OP tag，revm-inspectors沿用上游lock的0.43.0。

DeBank 可选 state-root 校验适配 Reth v2.6 取消 `HashedStorage.wiped` 的语义：Root A 使用父状态 provider 的 `hashed_post_state`，补齐 destroyed account 原有slot的零值；Root B 保留六字段RLP的完整严格解码与集合比较，依据decoded删除集合补父状态零slot，再覆盖decoded最终slot。replay bundle只提供被删账户的地址preimage，最终值来自decoded diff；缺失preimage报错。该处理保留旧 SELFDESTRUCT 和同块删除/重建语义，不改变线上 wire schema、validation hash、默认校验开关或投递路径。Receipt root调用移至新版Receipt关联函数。fixture同步适配SparseTrie epoch/清空接口及新state-gas字段。

## 3. 部署情况

测试主机 `lihe-dev`（当前实例 `i-0c8c99e496eda09b4`，ap-northeast-1a）。源为2026-11-06 03:02 UTC生产seed快照 `snap-03e41ac6ee45bf504`，对应 `production/blockchain-taiko` seed PV卷 `vol-004d459dc7d81d604`。本次测试卷 `vol-0a395b20d597fc805`，200GiB gp3、3000IOPS、125MiB/s、初始化300MiB/s，run tag `merge-reth-v1.5.0-20261106`；挂载 `/opt/apps/taiko-merge-v1.5.0/writer`，独立UUID，扩容后ext4 196GiB/使用107GiB/可用90GiB，未写fstab。初始化状态100% completed。启动时新版默认请求storage_v2，但正确沿用已有DB的stored=false；未转换原有存储格式。

仅部署 node + stock driver v2.7.0，与生产driver版本一致，隔离本轮Reth影响。node是RPC产出模式，无内嵌S3/Kafka投递；测试不运行ETL/jrpcx/通知服务，不注入生产AWS凭证，无需用 `is_backup` 改变执行路径。快照中的P2P身份与peerstore移至本run备份文件，生成独立node/client身份和JWT。RPC仅监听测试机回环，独立10.99.88.0/24网络；生产资源只读。

Compose全文：

```yaml
name: taiko-r150
services:
  node:
    image: public.ecr.aws/b2h7a5c4/chaintable/taiko-writer:ef478f27
    container_name: taiko-r150-node
    entrypoint:
    - /app/alethia-reth
    command:
    - node
    - --chain=mainnet
    - --datadir=/var/data
    - --config=/var/data/reth.toml
    - --p2p-secret-key=/var/data/alethia-p2p-secret
    - --bootnodes=enode://266a8e3b5e44201eca9c368d58aa59a7750295397e77d5b32aea2644f9962cbc4e1cb0543aab0480995a209408174413f65e5ce253d60bb83d22d3b8ab12eb89@34.142.239.251:30303,enode://264a7fc4bd1ee16cfc6eb420c643407bfc61b9c9534c5a39ba6e68c8759beda2fbeccefee8677385e3d99691eeb218da4bce7f5207cf38594ac0f6a53c128b9b@35.247.159.156:30303,enode://2d4e5b7ec0c57f9def6ebe72f9bd1f65c33c87b7dc38875bbb147c10e8ec9a8cd157558b695f9a02ac6ad789f300fab4f1f19d41273956491372e96880a3459f@34.126.90.255:30303,enode://57f4b29cd8b59dc8db74be51eedc6425df2a6265fad680c843be113232bbe632933541678783c2a5759d65eac2e2241c45a34e1c36254bccfe7f72e52707e561@104.197.107.1:30303,enode://87a68eef46cc1fe862becef1185ac969dfbcc050d9304f6be21599bfdcb45a0eb9235d3742776bc4528ac3ab631eba6816e9b47f6ee7a78cc5fcaeb10cd32574@35.232.246.122:30303,enode://27384375dbbfd8af5df71392fceb01b93cef1f674e4884fb481be145426f9ce5706941fd93928101a3079a52e621d8a42d266ab9165f4f960233adce22e905f8@34.21.195.34:30303
    - --port=30534
    - --discovery.port=30534
    - --enable-discv5-discovery
    - --discovery.v5.port=19231
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
    restart: on-failure:5
    stop_signal: SIGTERM
    stop_grace_period: 3m
    mem_limit: 12g
    cpus: 4
    volumes:
    - /opt/apps/taiko-merge-v1.5.0/writer:/var/data
    ports:
    - 127.0.0.1:28545:8545
    - 127.0.0.1:28660:6060
    - 30534:30534/tcp
    - 30534:30534/udp
    - 19231:19231/udp
    logging: &id001
      driver: json-file
      options:
        max-size: 100m
        max-file: '5'
    networks:
    - taiko-r150-test
  client:
    image: us-docker.pkg.dev/evmchain/images/taiko-client:taiko-alethia-client-v2.7.0@sha256:515bcec7e4870dd0ea11cabfb6797ed300f441deab0920b4c9f91b522193e7c0
    container_name: taiko-r150-client
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
    - --p2p.listen.tcp=19353
    - --p2p.listen.udp=19354
    - --p2p.useragent=taiko-merge-reth-v1.5.0
    - --p2p.bootnodes=enode://c263741b17759f3850d24d67d6c3cbc307c73e17d80c6b12a63a4792a10529d1125d00ecf7ef4c9b0dc51d28b94dfc1b8798fb524f61a1f93946748649f73b23@34.142.239.251:4001?discport=30304,enode://2f37c3affd83274b262fa2a259d32d41510dd5a48d6e916696efe7f1598cb3f905305f5989e7b6607aab50697fb2e52cb4b6904116ed67cc5fcea1e6d66ccaba@35.247.159.156:4001?discport=30304,enode://dd83dedeff622ecfca0c5edf320266506c811539a553ddd91589cdfcc9bbd74d0d620f251d8d5e1180f19a446abbdd8b6b5301e9aa6cbad35cfd9716f80f2416@34.126.90.255:4001?discport=30304,enode://0e917d0fd54e25cd9815aa552550c938dfccafb1b09e613d1b5f344236dc2a93cdac9508a9e570a4e1a315ca7762de51f41c4a90361519d972fdd6715bb3f4f3@51.210.195.167:4001?discport=30303,enode://0e917d0fd54e25cd9815aa552550c938dfccafb1b09e613d1b5f344236dc2a93cdac9508a9e570a4e1a315ca7762de51f41c4a90361519d972fdd6715bb3f4f3@34.248.16.142:4001?discport=30303,enode://67c739e00a9b6476fce8f9d3cf0ae12589c325c51520da75c2dbfe5094d5c9ef72abf0e368316b10e8332b96e1886869164113a86b66f7f8b989ed0d8dee8895@34.126.181.0:4001?discport=30304,enode://a31855cd5c4138327617bde2e87418774d03c40cd7217344e24f6d906a8ca72c777d65ed9bd38b20e03fa5f91302ae690b00963cd490011c549189ac24e0d1b9@34.21.195.34:4001?discport=30304
    - --p2p.priv.path=/var/data/client/p2p.key
    - --p2p.nat
    restart: on-failure:5
    stop_signal: SIGTERM
    stop_grace_period: 1m
    mem_limit: 2g
    cpus: 2
    depends_on:
    - node
    volumes:
    - /opt/apps/taiko-merge-v1.5.0/writer:/var/data
    ports:
    - 19353:19353/tcp
    - 19354:19354/udp
    - 127.0.0.1:29883:9871
    logging: *id001
    networks:
    - taiko-r150-test
networks:
  taiko-r150-test:
    driver: bridge
    ipam:
      config:
      - subnet: 10.99.88.0/24
```

## 4. 部署后测试情况

- 本地Rust1.99：RPC `cargo check --locked`、全workspace/all-features严格Clippy、nightly rustfmt/Cargo sort、diff检查通过。
- 全workspace/all-features原生nextest：385/385 PASS，0 skipped，执行7.091秒；依赖下载和编译另计。包含真实MDBX独立final-root（普通更新、slot清零、SELFDESTRUCT及重建）、主网历史anchor/普通交易fixture、Hoodi Unzen边界、EOA/EIP-7702、zk-gas tracing parity与取消/超时回归。
- PR #4 首轮CI [42176234528](https://github.com/Chaintable/alethia-reth/actions/runs/42176234528)：amd64、arm64与manifest全部成功。实际被测合成merge `ef478f27` 的tree与代码head `e112b2d` 完全一致（`ba4b867de8c0274a6f233d996792408546764392`）；后续仅追加原生测试API适配与本报告，生产Rust实现未变。镜像manifest digest为 `sha256:1c1bf59b64b3e555956e3e6065d5faff4fdbd75fa66757bc03d705f1fb7a306c`。
- 2026-11-06 06:58:59 UTC启动，RPC初始高度13,645,131；完成7,117块初始pipeline执行并追块，07:02:07 UTC采样与生产同为13,652,381，syncing=false，随后持续增长。node/client异常restart=0，OOM=false。主网运行中未发现state-root mismatch或节点崩溃。
- 快照末端13,645,112..13,645,131共20块，hash/stateRoot/receiptsRoot全部与生产一致。
- 固定近head连续100块13,652,586..13,652,685，100/100 hash/stateRoot/receiptsRoot与完整trace一致；state diff按六字段RLP解码后集合比较，包含所有账户、删除项、storage slot和code，不以header.stateRoot替代。仅忽略process_start_timestamp及既定集合顺序。共102笔交易（100笔anchor、2笔普通交易），全部为非空块；不是高交易量或主网全历史覆盖。执行器记录378.94秒（含本轮读取、等待及执行器续跑间隔），原生fixture和MDBX测试另计。
- 首个预扫描轮因选块老化触发128块参照窗口保护，22个hash一致、4个trace一致、18个窗口失效，未对失效样本调用历史trace；该轮保留为INVALID_REFERENCE_WINDOW_ABORTED，不计入100块通过数。最终100块使用固定当前/未来连续高度、两端到block+4后执行，并在每次生产trace前确认高度差≤128；没有替换失败用例。
- 07:16:23 UTC优雅停止，两个容器均exit=0/OOM=false；07:16:25 UTC重新启动。07:17:27 UTC观察测试13,652,841/生产13,652,842且继续增长、syncing=false。重启后固定13,652,833的hash/stateRoot/receiptsRoot及完整trace再次一致。
- 结论：本轮构建、原生回归、真实数据库恢复与追块、100块服务产物及优雅重启验证通过。生产切换仍由SRE执行；未merge PR、发布release、更新metadata或清理测试资源。

## 5. 过程中暴露的其他问题

生产client仍为v2.7.0，此前已独立验证的v2.8.0尚未上线；由SRE另行处理，不以本次Reth合并代替client升级。若未来启用proof-history，须单独规划首次v2重建/回填、磁盘增长、深重组背压以及回退前清理proof store；当前测试不证明该模式的主网长时间运行行为。
