# GRPC应用 Docker部署指南

本项目提供了一套完整的Docker部署解决方案，可以通过简单的命令快速部署整个应用程序。

## 前提条件

- 已安装 [Docker](https://www.docker.com/get-started)
- 已安装 [Docker Compose](https://docs.docker.com/compose/install/)

## 一键部署

项目提供了`deploy.sh`脚本，可以轻松管理应用的部署和操作。

### 使用方法

1. 为脚本添加执行权限：

```bash
chmod +x deploy.sh
```

2. 启动应用：

```bash
./deploy.sh start
```

这将自动构建并启动应用程序和Redis数据库。

### 其他命令

脚本还提供了以下命令：

- `./deploy.sh stop` - 停止应用
- `./deploy.sh restart` - 重启应用
- `./deploy.sh build` - 仅构建应用
- `./deploy.sh logs` - 查看应用日志
- `./deploy.sh status` - 查看应用状态
- `./deploy.sh help` - 显示帮助信息

## 手动管理

如果需要手动管理Docker容器，可以使用以下命令：

### 构建和启动

```bash
docker-compose up -d
```

### 停止服务

```bash
docker-compose down
```

### 查看日志

```bash
docker-compose logs -f app
```

## 配置说明

项目的配置文件为`config.toml`。在Docker环境中，Redis地址被设置为`redis://redis:6379/`，这对应于docker-compose.yml中定义的Redis服务名称。

如果需要修改配置，可以直接编辑`config.toml`文件，然后重新构建和启动容器：

```bash
./deploy.sh restart
```

## 目录说明

- `logs/` - 应用日志输出目录（挂载到容器中）
- `idls/` - IDL文件目录
- `src/` - 源代码目录
- `parsers/` - 解析器目录

## 数据持久化

Redis数据通过Docker卷进行持久化，即使容器重启，数据也不会丢失。 