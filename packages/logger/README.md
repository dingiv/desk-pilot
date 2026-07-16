# Unified Logger for JS packages of VRover

- every apps which use this Logger, should call `InitLogger` before `createLogger`.
- every modules should not call `createLogger` if they dont wanna call `InitLogger`. In such a condition, accepting a `Logger` instance from constructor/initializer is the only right implement.
