# 线程事件模型

fcitx 主循环
 {

    [keyborad, io_thread_eventsource]


    swift_ime {

        key -> swfit_ime_key
    }



 }


io_thread {
    [io_rx, ]

    io_rx -> 处理, -> io_thread_eventsource
}