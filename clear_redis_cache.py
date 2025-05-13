#!/usr/bin/env python
# -*- coding: utf-8 -*-

"""
清除Redis缓存中的所有数据
使用方法:
python clear_redis_cache.py [--host HOST] [--port PORT] [--password PASSWORD] [--db DB]
"""

import redis
import argparse
import sys

def clear_redis_cache(host='localhost', port=6379, password=None, db=0):
    """
    连接到Redis服务器并清除所有数据
    
    参数:
        host (str): Redis服务器地址，默认为localhost
        port (int): Redis服务器端口，默认为6379
        password (str): Redis服务器密码，默认为None
        db (int): Redis数据库编号，默认为0
    """
    try:
        # 连接到Redis服务器
        r = redis.Redis(host=host, port=port, password=password, db=db)
        
        # 测试连接
        r.ping()
        
        # 获取当前键数量
        keys_count = r.dbsize()
        print(f"当前Redis缓存中有 {keys_count} 个键")
        
        # 确认是否继续
        if keys_count > 0:
            confirm = input("确定要清除所有数据吗？(y/n): ")
            if confirm.lower() != 'y':
                print("操作已取消")
                return
            
            # 清除所有数据
            r.flushall()
            print("Redis缓存已清空")
        else:
            print("Redis缓存已经是空的")
            
    except redis.ConnectionError as e:
        print(f"无法连接到Redis服务器: {e}")
        sys.exit(1)
    except Exception as e:
        print(f"发生错误: {e}")
        sys.exit(1)

if __name__ == "__main__":
    # 解析命令行参数
    parser = argparse.ArgumentParser(description="清除Redis缓存中的所有数据")
    parser.add_argument("--host", default="localhost", help="Redis服务器地址，默认为localhost")
    parser.add_argument("--port", type=int, default=6379, help="Redis服务器端口，默认为6379")
    parser.add_argument("--password", default=None, help="Redis服务器密码，默认为None")
    parser.add_argument("--db", type=int, default=0, help="Redis数据库编号，默认为0")
    
    args = parser.parse_args()
    
    # 执行清除操作
    clear_redis_cache(args.host, args.port, args.password, args.db) 