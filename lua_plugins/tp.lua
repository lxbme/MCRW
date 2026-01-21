-- Support Minecraft 1.21.8+
print("[TP] Loading script...")

local tp_requests = {}

local function uuid()
    return tostring(os.time()) .. "-" .. tostring(math.random(1000, 9999))
end


wrapper:register(
    "\\[.*\\] \\[Server thread/INFO\\]: <(.*?)> !tp (.*)", 
    
    function(line, player, target)
        print("[TP] Detected !tp <?> command from " .. player)

        local req_id = uuid()
        
        tp_requests[req_id] = {
            initiator = player,
            target = target,
            timestamp = os.time()
        }

        return {
            "tellraw " .. player .. " {\"text\":\"[TP] Processing TP request...\",\"color\":\"yellow\"}",
            -- "tp " .. player .. " " .. target,
            'tellraw ' .. target .. ' ["",' ..
                '{"text":"' .. player .. ' wanna teleport to you. ","color":"gray","bold":false},' ..
                '{"text":"[ confirm ]","color":"green","bold":true,' ..
                'click_event:{action:"run_command",command:"/me confirm tp from ' .. player .. '"}}' ..
            ']'
        }
    end
)

wrapper:register(
    "\\[Server thread/INFO\\]: \\* (.*?) confirm tp from (.*)$",
    function(line, target, player)
        print("[TP] " .. target .. " confirmed TP from " .. player)
        
        local found = false
        for req_id, req_data in pairs(tp_requests) do
            if req_data.initiator == player and req_data.target == target then
            found = true
            tp_requests[req_id] = nil 
            break
            end
        end
        
        if found then
            return {
            "tp " .. player .. " " .. target,
            "tellraw " .. player .. " {\"text\":\"[TP] Teleporting...\",\"color\":\"green\"}",
            "tellraw " .. target .. " {\"text\":\"[TP] " .. player .. " teleported to you.\",\"color\":\"green\"}"
            }
        else
            return {
            -- "tellraw " .. target .. " {\"text\":\"[TP] No valid TP request found.\",\"color\":\"red\"}",
            "tellraw " .. player .. " {\"text\":\"[TP] TP request expired or invalid.\",\"color\":\"red\"}"
            }
        end
    end
)


wrapper:register(
    "\\[.*\\] \\[Server thread/INFO\\]: Teleported (.*?) to (.*)",
    function(line, p1, p2)
        return {
            "tellraw " .. p1 .. " {\"text\":\"[TP] Teleported you to " .. p2 .. ".\",\"color\":\"green\"}",
            "tellraw " .. p2 .. " {\"text\":\"[TP] Teleported " .. p1 .. " to you.\",\"color\":\"green\"}",
        }
    end
)

-- wrapper:register(
--     "\\[.*\\] \\[Server thread/INFO\\]: No player was found",
--     function(line)
--         return { "say [System] Someone tried to TP but failed!" }
--     end
-- )