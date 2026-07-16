# Agent module

## Style
推崇面向数据的作用域和生命周期编程。

当我们希望实现编码工程的时候，我们会倾向于将问题需要的核心状态用一个纯数据接口来表达，而其行为通过组合多个纯动作接口来表达，在其他模块依赖它时，优先通过接口进行依赖，而实现类则通过一个工厂函数进行隐藏；

```ts
export interface BehaviorA {
    fly(): void
    sound(): void
}

export interface BehaviorB {
    eat(): void
    drink(): void
}

export interface SomeAnimal extends BehaviorA, BehaviorB {
    name: string
    age: number
}

class SomeAnimalImpl implements SomeAnimal {
    // ...
    constructor() {
        // should be pure and effectless
    }
}

export function createSomeAnimal(): SomeAnimal {
    return new SomeAnimalImpl
}

export function destroySomeAnimal(ins) {
    // clear side effects and recycle some things
}


// some other modules

const ins = createSomeAnimal()

ins.fly()
ins.eat()

destroySomeAnimal(ins)

```
通过以上手段，我们在没有引入 IOC 容器的情况下，实现了基本的 SOLID 原则；