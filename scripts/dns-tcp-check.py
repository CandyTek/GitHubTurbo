import socket,struct,json
packet=struct.pack('!6H',0x6b0a,0x100,1,0,0,0)+b'\x06github\x03com\x00'+struct.pack('!2H',1,1)
def read(sock,n):
    data=b''
    while len(data)<n:
        chunk=sock.recv(n-len(data))
        if not chunk:raise EOFError()
        data+=chunk
    return data
def skip(data,pos):
    while data[pos]:
        if data[pos]&0xc0==0xc0:return pos+2
        pos+=data[pos]+1
    return pos+1
for server in ['8.8.8.8','119.29.29.29','223.5.5.5']:
    try:
        with socket.create_connection((server,53),timeout=2) as s:
            s.sendall(struct.pack('!H',len(packet))+packet)
            data=read(s,struct.unpack('!H',read(s,2))[0]);header=struct.unpack('!6H',data[:12]);pos=12;ips=[]
            for _ in range(header[2]):pos=skip(data,pos)+4
            for _ in range(header[3]):
                pos=skip(data,pos);kind,cls,ttl,n=struct.unpack('!HHIH',data[pos:pos+10]);pos+=10
                if kind==1 and n==4:ips.append(socket.inet_ntoa(data[pos:pos+n]))
                pos+=n
            print(json.dumps({'server':server,'ips':ips}),flush=True)
    except Exception as e:print(json.dumps({'server':server,'error':str(e)}),flush=True)
