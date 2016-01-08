  $ $PYTHON -c 'import remotenames' || exit 80
  $ extpath=$(dirname $TESTDIR)
  $ cat > $TESTTMP/pretxnchangegroup.sh << EOF
  > #!/bin/bash
  > env | grep -E "^HG_USERVAR"
  > exit 0
  > EOF
  $ chmod +x $TESTTMP/pretxnchangegroup.sh
  $ cp $extpath/pushvars.py $TESTTMP # use $TESTTMP substitution in message
  $ cat >> $HGRCPATH << EOF
  > [extensions]
  > pushvars=$TESTTMP/pushvars.py
  > remotenames=
  > [hooks]
  > pretxnchangegroup = $TESTTMP/pretxnchangegroup.sh
  > EOF

  $ hg init server
  $ cd server

  $ echo x > x
  $ hg commit -qAm x
  $ hg book master

  $ cd ..
  $ hg clone -q server client
  $ cd client
  $ echo x >> x
  $ hg commit -m x

Remotenames should not interfere with pushvars

  $ hg push --to master --debug --pushvars MYPUSHVAR=true 2>&1 | egrep -i '(USERVAR|pushvar)'
  pushing rev c73f3db8c9d2 to destination $TESTTMP/server bookmark master
  bundle2-output-part: "pushvars" (params: 0 advisory) empty payload
  bundle2-input-part: "pushvars" (params: 0 advisory) supported
  running hook pretxnchangegroup: $TESTTMP/pretxnchangegroup.sh
  HG_USERVAR_MYPUSHVAR=true
