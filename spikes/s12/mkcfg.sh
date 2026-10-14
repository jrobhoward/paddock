#!/usr/bin/env bash
# Writes the TLC configs: name, comment, then the fault and fix switches.
common='SPECIFICATION Spec

CONSTRAINT Bounds
INVARIANT TypeOK
INVARIANT LocalLive
INVARIANT Agreement
SYMMETRY Symmetry
'
mk() { # name comment cut close crash spurious qfull drop fixnotice fixunlink maxcr fixclose [fixcontact]
cat > "$1.cfg" <<EOF
\* $2
$common
CONSTANTS
    A = A
    B = B
    MaxInc = 2
    MaxCr = ${11}
    MaxRef = 1
    MaxSession = 3
    MaxChan = 2
    MaxFaults = 2
    CanCut = $3
    CanClose = $4
    CanCrash = $5
    CanSpurious = $6
    CanQueueFull = $7
    CanDrop = $8
    FixFailedNotice = $9
    FixUnlinkAck = ${10}
    FixReplacedClose = ${12}
    FixContactFirst = ${13:-FALSE}
EOF
}
mk Quic "QUIC as built: stalls, closes, spurious timeouts; frames lost only with the connection." TRUE TRUE FALSE TRUE FALSE FALSE FALSE FALSE 1 FALSE
mk QuicUnlinkAck "As Quic, with OTP 23's acknowledged unlink." TRUE TRUE FALSE TRUE FALSE FALSE FALSE TRUE 1 FALSE
mk QuicFixed "As QuicTwoFixes, and a failed notice ends the session." TRUE TRUE FALSE TRUE FALSE FALSE TRUE TRUE 1 TRUE
mk CrashesR1R3 "As QuicFixed, and a node may crash and restart with a new creation; R1 to R3 only." TRUE TRUE TRUE TRUE FALSE FALSE TRUE TRUE 2 TRUE FALSE
mk Crashes "As QuicFixed, and a node may crash and restart with a new creation; R1 to R4." TRUE TRUE TRUE TRUE FALSE FALSE TRUE TRUE 2 TRUE TRUE
mk QueueFull "Full send queues only, with the acknowledged unlink but not the failed-notice fix." FALSE FALSE FALSE FALSE TRUE FALSE FALSE TRUE 1 FALSE
mk QueueFullFixed "As QueueFull, with the three fixes." FALSE FALSE FALSE TRUE TRUE FALSE TRUE TRUE 1 TRUE
mk Drop "Frames lost while the session goes on (the mesh's drop), with the three fixes." FALSE FALSE FALSE FALSE FALSE TRUE TRUE TRUE 1 TRUE
mk NoR1 "As QuicFixed without R1: shows R1 is needed." TRUE TRUE FALSE TRUE FALSE FALSE FALSE TRUE 1 TRUE
mk NoR2 "As QuicFixed without R2: shows R2 is needed." TRUE TRUE FALSE TRUE FALSE FALSE TRUE TRUE 1 FALSE
mk NoR3 "As QuicFixed without R3: shows R3 is needed." TRUE TRUE FALSE TRUE FALSE FALSE TRUE FALSE 1 TRUE
