
---
name: hello
comment: 显示在候选区域
params: name, age
---
```
hello, my name is ${name}. I am ${age} years old.
```

---
name: angle
comment: print a angle with a char
---
```
   ${PATH_VAR}
  ${PATH_VAR}
 ${PATH_VAR}
${PATH_VAR}${PATH_VAR}${PATH_VAR}${PATH_VAR}
```


---
name: env
comment: get an env variable
---
```
your name is ${ENV:USERNAME}
your env ${PATH_VAR} is ${ENV:$PATH_VAR}
```

下面讲解这些snippet的用法, 路径参数的使用
param 的使用, #/hello?name=Mike&age=18, 展开结果, 不包括首行换行符, 不包括结尾换行符;

hello, my name is ${name}. I am ${age} years old.

#/angle/O

输出如下:

    O
O
O
OOOO

#/env/HOME   , 不包括首行换行符, 不包括结尾换行符, 但是包括中间的那个换行符号

your name is someuser
your env HOME is /home/someuser