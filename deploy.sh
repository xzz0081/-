#!/bin/bash

# 显示彩色文本的函数
print_color() {
  case $1 in
    "red") COLOR='\033[0;31m' ;;
    "green") COLOR='\033[0;32m' ;;
    "yellow") COLOR='\033[0;33m' ;;
    "blue") COLOR='\033[0;34m' ;;
    "cyan") COLOR='\033[0;36m' ;;
    *) COLOR='\033[0m' ;;
  esac
  NC='\033[0m' # 无颜色
  printf "${COLOR}$2${NC}\n"
}

# 检查Docker是否已安装
check_docker() {
  if ! command -v docker &> /dev/null; then
    print_color "red" "错误: Docker未安装! 请先安装Docker."
    exit 1
  fi

  if ! command -v docker-compose &> /dev/null; then
    print_color "red" "错误: Docker Compose未安装! 请先安装Docker Compose."
    exit 1
  fi
}

# 启动应用
start_app() {
  print_color "cyan" "正在启动应用..."
  docker-compose up -d
  if [ $? -eq 0 ]; then
    print_color "green" "应用已成功启动!"
    print_color "blue" "您可以使用以下命令查看日志:"
    echo "  docker-compose logs -f app"
  else
    print_color "red" "启动应用时出错!"
  fi
}

# 停止应用
stop_app() {
  print_color "cyan" "正在停止应用..."
  docker-compose down
  if [ $? -eq 0 ]; then
    print_color "green" "应用已成功停止!"
  else
    print_color "red" "停止应用时出错!"
  fi
}

# 重启应用
restart_app() {
  print_color "cyan" "正在重启应用..."
  docker-compose restart
  if [ $? -eq 0 ]; then
    print_color "green" "应用已成功重启!"
  else
    print_color "red" "重启应用时出错!"
  fi
}

# 构建应用
build_app() {
  print_color "cyan" "正在构建应用..."
  docker-compose build
  if [ $? -eq 0 ]; then
    print_color "green" "应用已成功构建!"
  else
    print_color "red" "构建应用时出错!"
  fi
}

# 查看日志
view_logs() {
  print_color "cyan" "显示应用日志..."
  docker-compose logs -f app
}

# 检查应用状态
check_status() {
  print_color "cyan" "检查应用状态..."
  docker-compose ps
}

# 显示帮助信息
show_help() {
  echo "GRPC应用一键部署脚本"
  echo
  echo "用法: ./deploy.sh [命令]"
  echo
  echo "可用命令:"
  echo "  start      启动应用"
  echo "  stop       停止应用"
  echo "  restart    重启应用"
  echo "  build      构建应用"
  echo "  logs       查看日志"
  echo "  status     检查状态"
  echo "  help       显示帮助信息"
  echo
  echo "如果没有提供命令，默认执行'start'命令。"
}

# 主函数
main() {
  check_docker

  case "$1" in
    "start")
      start_app
      ;;
    "stop")
      stop_app
      ;;
    "restart")
      restart_app
      ;;
    "build")
      build_app
      ;;
    "logs")
      view_logs
      ;;
    "status")
      check_status
      ;;
    "help")
      show_help
      ;;
    "")
      start_app
      ;;
    *)
      print_color "red" "未知命令: $1"
      show_help
      exit 1
      ;;
  esac
}

# 执行主函数
main "$@" 