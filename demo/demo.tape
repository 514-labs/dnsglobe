# vhs tape for the README demo — regenerate with `vhs demo/demo.tape`
Output demo/demo.gif

Set Shell "bash"
Set FontSize 13
Set Width 1600
Set Height 780
Set Padding 12
Set TypingSpeed 60ms

Hide
Type "clear"
Enter
Show

# real ISP subnets (Comcast US, Telstra AU) — documentation ranges like
# 203.0.113.0/24 get REFUSED by Google when sent as ECS
Type "dnsglobe 514.ax --ecs 73.98.0.0/24,1.128.0.0/24"
Sleep 800ms
Enter
# initial A-record round streams in with the first ECS subnet active;
# resolvers that ignore ECS get tagged NO ECS
Sleep 7s

# cycle to the second ECS client subnet — re-queries immediately
Ctrl+N
Sleep 6s

# switch to NS and re-query to check the nameservers
Tab
Tab
Tab
Tab
Sleep 6s

# morph the flat map into the rotating globe
Ctrl+O
Sleep 6s
