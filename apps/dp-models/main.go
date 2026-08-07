// dp-models — desk-pilot 模型服务进程(独立进程形态)。
//
// 以 SDK 方式引入 LocalAI(不复制源码),库式启动:
//   application.New(opts...) → http.API(app) → Start(addr)
// 模型声明在 models/ 目录(YAML, LocalAI 格式: name/backend/parameters.model)。
//
// 用法: go run . [--models dir] [--addr :8080]
package main

import (
	"context"
	"flag"
	"log"
	"path/filepath"

	"github.com/mudler/LocalAI/core/application"
	"github.com/mudler/LocalAI/core/config"
	"github.com/mudler/LocalAI/core/http"
	"github.com/mudler/LocalAI/pkg/modelartifacts"
	"github.com/mudler/LocalAI/pkg/system"
)

func main() {
	modelsPath := flag.String("models", "models", "模型声明目录(含 *.yaml)")
	addr := flag.String("addr", ":8080", "监听地址")
	flag.Parse()

	// 与 LocalAI CLI 相同的启动序列(RunCMD.Run 的最小复刻):
	//   system state(模型/后端路径) → AppOption 链 → application.New → http.API → Start
	systemState, err := system.GetSystemState(
		system.WithModelPath(*modelsPath),
		system.WithBackendPath(filepath.Join(*modelsPath, "backend-assets")),
		system.WithBackendSystemPath(filepath.Join(*modelsPath, "backend-assets")),
	)
	if err != nil {
		log.Fatalf("system state: %v", err)
	}

	opts := []config.AppOption{
		config.WithContext(context.Background()),
		config.WithSystemState(systemState),
		config.WithConfigFile(filepath.Join(*modelsPath, "models.yaml")),
		config.WithModelArtifactMaterializer(modelartifacts.NewDefaultManager()),
	}

	app, err := application.New(opts...)
	if err != nil {
		log.Fatalf("application.New: %v", err)
	}

	appHTTP, err := http.API(app)
	if err != nil {
		log.Fatalf("http.API: %v", err)
	}

	log.Printf("dp-models (LocalAI) 启动: %s, models=%s", *addr, *modelsPath)
	if err := appHTTP.Start(*addr); err != nil {
		log.Fatalf("server: %v", err)
	}
}
