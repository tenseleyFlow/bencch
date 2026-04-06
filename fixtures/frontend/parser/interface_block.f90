program parse_interface
  implicit none
  interface
    subroutine ping(value)
      integer, intent(in) :: value
    end subroutine ping
  end interface
  print *, 1
end program parse_interface
