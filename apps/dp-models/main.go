// dp-models — desk-pilot 模型服务进程(独立进程形态)。
//
// 以 Go module 方式引入 LocalAI SDK(v1.40.0, github.com/go-skynet/LocalAI),
// 库式启动: api.App(opts...) → fiber App → Listen(addr)。
// 模型声明在 models/ 目录(LocalAI YAML 格式: name/backend/parameters.model);
// 后端二进制(如 llama-cpp)在首次加载模型时从 gallery 按需下载。
//
// 用法: go run . [--models dir] [--addr :8080]
package main

import (
	"context"
	"flag"
	"log"
	"path/filepath"

	api "github.com/go-skynet/LocalAI/api"
	"github.com/go-skynet/LocalAI/api/options"
	"github.com/go-skynet/LocalAI/pkg/model"
)

func main() {
	modelsPath := flag.String("models", "models", "模型声明目录(含 *.yaml)")
	addr := flag.String("addr", ":8080", "监听地址")
	threads := flag.Int("threads", 8, "推理线程数")
	flag.Parse()

	opts := []options.AppOption{
		options.WithContext(context.Background()),
		options.WithModelLoader(model.NewModelLoader(*modelsPath)),
		options.WithConfigFile(filepath.Join(*modelsPath, "models.yaml")),
		options.WithThreads(*threads),
	}

	app, err := api.App(opts...)
	if err != nil {
		log.Fatalf("api.App: %v", err)
	}

	log.Printf("dp-models (LocalAI) 启动: %s, models=%s", *addr, *modelsPath)
	if err := app.Listen(*addr); err != nil {
		log.Fatalf("server: %v", err)
	}
}
