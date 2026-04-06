program parse_select_type
  implicit none
  type :: shape
    integer :: sides
  end type shape

  type, extends(shape) :: square
    integer :: edge
  end type square

  type(square) :: item
  item%sides = 4
  item%edge = 9

  select type (item)
  type is (square)
    print *, item%edge
  class default
    print *, 0
  end select
end program parse_select_type
