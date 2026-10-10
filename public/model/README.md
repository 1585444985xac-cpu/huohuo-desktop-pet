# 这里放 Live2D 模型（本仓库**不含**模型文件）

程序启动时加载的是：

```
public/model/character/huohuo.model3.json
```

（见 `src/App.vue` 里 `Live2DModel.from('/model/character/huohuo.model3.json')`。）

## 为什么仓库里没有模型

模型自带的使用说明（`instructions-使用说明.txt`）写得很清楚：

> 1. The copyright of this model belongs to miHoYo
> 3. … **secondary distribution is prohibited**（禁止二次分发）

也就是说，模型不是本项目的作者能再分发的素材，所以它被 `.gitignore` 排除在外。
克隆本仓库后，**请自己准备一份合法的模型**放进 `character/` 目录，文件名对齐上面的路径即可。

## 需要哪些文件

`huohuo.model3.json` 会引用同目录下的其余文件，通常包括：

- `huohuo.moc3`（模型本体）
- `huohuo.8192/texture_*.png`（贴图）
- `*.motion3.json`（动作）、`*.exp3.json`（表情）、`huohuo.physics3.json`、`huohuo.cdi3.json`

## 本项目作者自己的用法

本机保留了一份模型用于开发与自用，它只在本地目录里、不进版本库；
如果你打算把本项目公开，请务必保持这个目录不出现在公开仓库中。
