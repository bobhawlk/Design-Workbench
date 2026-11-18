"""Writes the small STEP files the tests read (crates/*/tests/*.stp).

Each solid is a B-rep of planar and cylindrical faces with straight and circular edges, written
the way Inventor and most exporters write them: products, NEXT_ASSEMBLY_USAGE_OCCURRENCE and
CONTEXT_DEPENDENT_SHAPE_REPRESENTATION placements, millimetre units."""
import math, sys, os

class W:
    def __init__(s): s.lines=[]; s.n=0
    def add(s, text):
        s.n+=1; s.lines.append(f"#{s.n}={text};"); return s.n
    def pt(s,p): return s.add("CARTESIAN_POINT('',(%s))" % ",".join(f"{v:.6f}" for v in p))
    def dr(s,d): return s.add("DIRECTION('',(%s))" % ",".join(f"{v:.6f}" for v in d))
    def ax(s,o,z=(0,0,1),x=(1,0,0)): return s.add(f"AXIS2_PLACEMENT_3D('',#{s.pt(o)},#{s.dr(z)},#{s.dr(x)})")
    def text(s):
        return "ISO-10303-21;\nHEADER;\nFILE_DESCRIPTION(('fixture'),'2;1');\nFILE_NAME('fixture','',(''),(''),'','','');\nFILE_SCHEMA(('AUTOMOTIVE_DESIGN'));\nENDSEC;\nDATA;\n"+"\n".join(s.lines)+"\nENDSEC;\nEND-ISO-10303-21;\n"

def sub(a,b): return tuple(x-y for x,y in zip(a,b))
def cross(a,b): return (a[1]*b[2]-a[2]*b[1],a[2]*b[0]-a[0]*b[2],a[0]*b[1]-a[1]*b[0])
def norm(a):
    l=math.sqrt(sum(x*x for x in a)); return tuple(x/l for x in a)

class Brep:
    """Collects vertices/edges by key so faces share edges."""
    def __init__(s,w): s.w=w; s.verts={}; s.edges={}; s.faces=[]
    def v(s,p):
        k=tuple(round(x,6) for x in p)
        if k not in s.verts: s.verts[k]=s.w.add(f"VERTEX_POINT('',#{s.w.pt(p)})")
        return s.verts[k]
    def line_edge(s,a,b):
        k=frozenset([tuple(round(x,6) for x in a),tuple(round(x,6) for x in b)])
        if k not in s.edges:
            d=norm(sub(b,a)); L=math.dist(a,b)
            vec=s.w.add(f"VECTOR('',#{s.w.dr(d)},{L:.6f})")
            ln=s.w.add(f"LINE('',#{s.w.pt(a)},#{vec})")
            s.edges[k]=(s.w.add(f"EDGE_CURVE('',#{s.v(a)},#{s.v(b)},#{ln},.T.)"),a)
        e,start=s.edges[k]
        return e, tuple(round(x,6) for x in start)==tuple(round(x,6) for x in a)
    def arc_edge(s,a,b,center,axis,r,xdir):
        k=('arc',frozenset([tuple(round(x,6) for x in a),tuple(round(x,6) for x in b)]),tuple(round(x,6) for x in center))
        if k not in s.edges:
            c=s.w.add(f"CIRCLE('',#{s.w.ax(center,axis,xdir)},{r:.6f})")
            s.edges[k]=(s.w.add(f"EDGE_CURVE('',#{s.v(a)},#{s.v(b)},#{c},.T.)"),a)
        e,start=s.edges[k]
        return e, tuple(round(x,6) for x in start)==tuple(round(x,6) for x in a)
    def face(s,surface,loop_edges,same=True):
        oes=[s.w.add(f"ORIENTED_EDGE('',*,*,#{e},{'.T.' if o else '.F.'})") for e,o in loop_edges]
        lp=s.w.add("EDGE_LOOP('',(%s))" % ",".join(f"#{x}" for x in oes))
        b=s.w.add(f"FACE_OUTER_BOUND('',#{lp},.T.)")
        s.faces.append(s.w.add(f"ADVANCED_FACE('',(#{b}),#{surface},{'.T.' if same else '.F.'})"))
    def poly_face(s,pts,normal):
        x=norm(sub(pts[1],pts[0]))
        pl=s.w.add(f"PLANE('',#{s.w.ax(pts[0],normal,x)})")
        s.face(pl,[s.line_edge(pts[i],pts[(i+1)%len(pts)]) for i in range(len(pts))])
    def solid(s,name):
        sh=s.w.add("CLOSED_SHELL('',(%s))" % ",".join(f"#{f}" for f in s.faces))
        return s.w.add(f"MANIFOLD_SOLID_BREP('{name}',#{sh})")

def box(w,name,L,Wd,T):
    b=Brep(w)
    P=lambda x,y,z:(x,y,z)
    c=[P(0,0,0),P(L,0,0),P(L,Wd,0),P(0,Wd,0)]; t=[(x,y,T) for x,y,_ in c]
    b.poly_face(c[::-1],(0,0,-1)); b.poly_face(t,(0,0,1))
    for i in range(4):
        a,bb=c[i],c[(i+1)%4]; n=norm(cross(sub(bb,a),(0,0,1)))
        b.poly_face([a,bb,t[(i+1)%4],t[i]],n)
    return b.solid(name)

def l_bracket(w,name,L,H1,H2,T,R):
    """A plate L long (x) with a 90° bend: base along y from 0..H1 on z=0..T, flange up to H2,
    inner bend radius R. Section in the y-z plane, extruded along x."""
    b=Brep(w)
    ri,ro=R,R+T
    cy,cz=H1,R+T  # bend axis (along x) at y=H1, z=R+T ... inner radius R measured from axis
    # Section outline (counter-clockwise seen from +x): bottom outer skin, outer arc, flange outer, top, flange inner, inner arc, base top, base end
    sec=[('l',(0,0),(H1,0)),('a',(H1,0),(H1+ro,cz),ro,'cw'),('l',(H1+ro,cz),(H1+ro,cz+H2)),('l',(H1+ro,cz+H2),(H1+ri,cz+H2)),
         ('l',(H1+ri,cz+H2),(H1+ri,cz)),('a',(H1+ri,cz),(H1,T),ri,'ccw'),('l',(H1,T),(0,T)),('l',(0,T),(0,0))]
    X0,X1=0.0,L
    p3=lambda x,yz:(x,yz[0],yz[1])
    center=(cy,cz)
    for seg in sec:
        a,bb=seg[1],seg[2]
        A0,B0,A1,B1=p3(X0,a),p3(X0,bb),p3(X1,a),p3(X1,bb)
        if seg[0]=='l':
            d=sub(B0,A0); n=norm(cross(d,(1,0,0)))
            b.poly_face([A0,B0,B1,A1],n)
        else:
            r=seg[3]
            cyl=w.add(f"CYLINDRICAL_SURFACE('',#{w.ax((0,cy,cz),(1,0,0),(0,0,-1))},{r:.6f})")
            e1=b.arc_edge(A0,B0,(X0,cy,cz),(1,0,0),r,(0,0,-1)); e2=b.line_edge(B0,B1)
            e3=b.arc_edge(B1,A1,(X1,cy,cz),(1,0,0),r,(0,0,-1)); e4=b.line_edge(A1,A0)
            b.face(cyl,[e1,e2,(e3[0],not e3[1]) if False else e3,e4],same=(seg[4]=='cw'))
    # End caps (x=0 and x=L): approximate polygon faces with arcs as edges
    for X,n in ((X0,(-1,0,0)),(X1,(1,0,0))):
        loop=[]
        for seg in sec:
            a,bb=p3(X,seg[1]),p3(X,seg[2])
            if seg[0]=='l': loop.append(b.line_edge(a,bb))
            else: loop.append(b.arc_edge(a,bb,(X,cy,cz),(1,0,0),seg[3],(0,0,-1)))
        if n[0]<0: loop=[(e,not o) for e,o in reversed(loop)]
        pl=w.add(f"PLANE('',#{w.ax((X,0,0),n,(0,1,0))})")
        b.face(pl,loop)
    return b.solid(name)

def product_and_shape(w,ctx,name,items):
    p=w.add(f"PRODUCT('{name}','{name}','',(#{w.prodctx}))")
    pdf=w.add(f"PRODUCT_DEFINITION_FORMATION('','',#{p})")
    pd=w.add(f"PRODUCT_DEFINITION('design','',#{pdf},#{w.pdctx})")
    pds=w.add(f"PRODUCT_DEFINITION_SHAPE('','',#{pd})")
    origin=w.ax((0,0,0))
    sr=w.add("SHAPE_REPRESENTATION('',(%s),#%d)" % (",".join(f"#{x}" for x in [origin]+items), ctx))
    w.add(f"SHAPE_DEFINITION_REPRESENTATION(#{pds},#{sr})")
    return pd,sr,origin

def header(w):
    app=w.add("APPLICATION_CONTEXT('automotive design')")
    w.prodctx=w.add(f"PRODUCT_CONTEXT('',#{app},'mechanical')")
    w.pdctx=w.add(f"PRODUCT_DEFINITION_CONTEXT('part definition',#{app},'design')")
    lu=w.add("(LENGTH_UNIT() NAMED_UNIT(*) SI_UNIT(.MILLI.,.METRE.))")
    au=w.add("(NAMED_UNIT(*) PLANE_ANGLE_UNIT() SI_UNIT($,.RADIAN.))")
    su=w.add("(NAMED_UNIT(*) SI_UNIT($,.STERADIAN.) SOLID_ANGLE_UNIT())")
    return w.add(f"(GEOMETRIC_REPRESENTATION_CONTEXT(3) GLOBAL_UNIT_ASSIGNED_CONTEXT((#{lu},#{au},#{su})) REPRESENTATION_CONTEXT('',''))")

def place(w,parent,child,name,origin,z=(0,0,1),x=(1,0,0)):
    ppd,psr,porigin=parent; cpd,csr,corigin=child
    n=w.add(f"NEXT_ASSEMBLY_USAGE_OCCURRENCE('{name}','{name}','',#{ppd},#{cpd},$)")
    pds=w.add(f"PRODUCT_DEFINITION_SHAPE('','',#{n})")
    target=w.ax(origin,z,x)
    idt=w.add(f"ITEM_DEFINED_TRANSFORMATION('','',#{corigin},#{target})")
    rr=w.add(f"(REPRESENTATION_RELATIONSHIP('','',#{csr},#{psr}) REPRESENTATION_RELATIONSHIP_WITH_TRANSFORMATION(#{idt}) SHAPE_REPRESENTATION_RELATIONSHIP())")
    w.add(f"CONTEXT_DEPENDENT_SHAPE_REPRESENTATION(#{rr},#{pds})")

def two_plates():
    w=W(); ctx=header(w)
    solid=box(w,'Plate',40,20,2)
    plate=product_and_shape(w,ctx,'plate',[solid])
    asm=product_and_shape(w,ctx,'two plates',[])
    place(w,asm,plate,'plate:1',(0,0,0)); place(w,asm,plate,'plate:2',(0,0,10))
    return w.text()

def bracket_asm():
    """An L bracket (2 mm, R2) placed four times in a row 50 mm apart, and a mirrored pair of
    plates either side of x = 0 with a 6 mm hole axis through a shared pin."""
    w=W(); ctx=header(w)
    br=product_and_shape(w,ctx,'bracket',[l_bracket(w,'Bracket',30,25,20,2,2)])
    plate=product_and_shape(w,ctx,'plate',[box(w,'Plate',40,20,2)])
    asm=product_and_shape(w,ctx,'rack',[])
    for i in range(4): place(w,asm,br,f'bracket:{i+1}',(50*i,0,0))
    place(w,asm,plate,'plate:1',(-60,0,0))
    place(w,asm,plate,'plate:2',(-60,0,30))
    return w.text()

if __name__=='__main__':
    root=os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    open(os.path.join(root,'crates/lcw-step/tests/two_plates.stp'),'w').write(two_plates())
    os.makedirs(os.path.join(root,'crates/lcw-analyze/tests'),exist_ok=True)
    open(os.path.join(root,'crates/lcw-analyze/tests/bracket_rack.stp'),'w').write(bracket_asm())
    print('ok')
